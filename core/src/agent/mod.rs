pub mod config;
mod entity;
pub mod error;
mod pi_export;
pub mod repo;
pub mod session;
mod system_prompt;

use std::sync::Arc;

use crate::audit::Audit;
use crate::note::Notes;
use crate::skill::Skills;
use crate::toolset::ToolSets;

/// Lead → `WorkspaceAdmin`; Agent → `WorkspaceMember` (read-only). Sandbox
/// scopes are added later via [`Agent::sandbox_attached`].
fn default_authz_scopes(role: AgentRole, workspace_id: WorkspaceId) -> Vec<AuthScope> {
    match role {
        AgentRole::WorkspaceLead => vec![AuthScope::WorkspaceAdmin(workspace_id)],
        AgentRole::Agent => vec![AuthScope::WorkspaceMember(workspace_id)],
    }
}

use tracing::instrument;

use crate::primitives::{
    AgentId, AuthResource, AuthScope, AuthSubject, AuthVerb, ChatOutputEvent, ContextGeneration,
    SandboxId, WorkflowDefinitionId, WorkflowRunId, WorkspaceId,
};
use crate::sandbox::{SandboxAgentMode, Sandboxes};
pub use config::{AgentsConfig, ModelDefaults, RoleConfig};
pub use entity::*;
pub use error::AgentError;
use repo::AgentRepo;
use session::Sessions;

/// Snapshot of dynamic system blocks (notes + skills), keyed by
/// `ContextGeneration` at fetch time. Skips DB round-trips on the hot path.
/// Skills are scoped to `(workspace_id, attached_sandbox_id)` because each
/// agent's exported sandbox skills depend on its own attachment.
#[derive(Clone)]
struct CachedAgentContext {
    generation: u64,
    notes_block: Option<session::message::SystemBlock>,
    skills_block: Option<session::message::SystemBlock>,
}

impl CachedAgentContext {
    fn to_blocks(&self) -> Vec<session::message::SystemBlock> {
        let mut out = Vec::with_capacity(2);
        if let Some(b) = &self.notes_block {
            out.push(b.clone());
        }
        if let Some(b) = &self.skills_block {
            out.push(b.clone());
        }
        out
    }
}

#[derive(Clone)]
pub struct Agents {
    repo: AgentRepo,
    sessions: Sessions,
    sandboxes: Arc<Sandboxes>,
    skills: Arc<Skills>,
    notes: Option<Arc<Notes>>,
    config: AgentsConfig,
    toolsets: Arc<ToolSets>,
    prompt_requests: llm::PromptRequestChannel,
    context_generation: ContextGeneration,
    context_cache: ContextCache,
}

type ContextCacheKey = (WorkspaceId, Option<SandboxId>);
type ContextCache =
    Arc<std::sync::RwLock<std::collections::HashMap<ContextCacheKey, CachedAgentContext>>>;

impl Agents {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: &sqlx::PgPool,
        config: AgentsConfig,
        toolsets: Arc<ToolSets>,
        prompt_requests: llm::PromptRequestChannel,
        sandboxes: Arc<Sandboxes>,
        skills: Arc<Skills>,
        notes: Option<Arc<Notes>>,
        context_generation: ContextGeneration,
    ) -> Self {
        Self {
            repo: AgentRepo::new(pool),
            sessions: Sessions::new(pool),
            sandboxes,
            skills,
            notes,
            config,
            toolsets,
            prompt_requests,
            context_generation,
            context_cache: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
        }
    }

    /// Hot-path lookup of dynamic system blocks. Reads `ContextGeneration`
    /// atomically; only hits DB when the generation has bumped. Skills are
    /// scoped to the agent's attached sandbox so two agents in the same
    /// workspace with different sandboxes don't share an entry.
    async fn cached_dynamic_blocks(
        &self,
        workspace_id: WorkspaceId,
        sandbox_id: Option<SandboxId>,
    ) -> Vec<session::message::SystemBlock> {
        let current_gen = self.context_generation.current();
        let key = (workspace_id, sandbox_id);

        {
            let cache = self.context_cache.read().expect("context_cache poisoned");
            if let Some(cached) = cache.get(&key) {
                if cached.generation == current_gen {
                    return cached.to_blocks();
                }
            }
        }

        let notes_block = match &self.notes {
            Some(notes) => notes
                .pinned_context_for_workspace(workspace_id)
                .await
                .ok()
                .flatten()
                .map(|text| session::message::SystemBlock::Notes { text }),
            None => None,
        };
        let skills_block = self
            .skills
            .skills_context_for_agent(workspace_id, sandbox_id)
            .await
            .ok()
            .flatten()
            .map(|text| session::message::SystemBlock::Skills { text });

        let entry = CachedAgentContext {
            generation: current_gen,
            notes_block: notes_block.clone(),
            skills_block: skills_block.clone(),
        };
        let result = entry.to_blocks();
        if let Ok(mut cache) = self.context_cache.write() {
            cache.insert(key, entry);
        }
        result
    }

    pub fn skills(&self) -> &Skills {
        &self.skills
    }

    /// For composing `*_in_op` methods with caller-driven writes.
    pub async fn begin_op(&self) -> Result<es_entity::DbOp<'_>, sqlx::Error> {
        self.repo.begin_op().await
    }

    #[instrument(name = "domain.agent.create_workspace_lead", skip(self, sub))]
    pub async fn create_workspace_lead(
        &self,
        sub: &AuthSubject,
        workspace_id: WorkspaceId,
        name: impl Into<String> + std::fmt::Debug,
        workspace_name: &str,
    ) -> Result<Agent, AgentError> {
        sub.can(AuthVerb::Create, AuthResource::Agent(workspace_id, None))?;
        Audit::record_action_if_unset("agent.create_workspace_lead");
        Audit::record_workspace_id(workspace_id);
        let id = AgentId::new();
        Audit::record_agent_id(id);
        let mut op = self.repo.begin_op().await?;
        let agent = self
            .create_in_op(
                &mut op,
                id,
                workspace_id,
                AgentRole::WorkspaceLead,
                name,
                None,
                workspace_name,
                None,
                None,
            )
            .await?;
        op.commit().await?;
        Ok(agent)
    }

    /// Workspace name is resolved from the existing lead agent.
    #[instrument(name = "domain.agent.create_agent", skip(self, sub))]
    pub async fn create_agent(
        &self,
        sub: &AuthSubject,
        workspace_id: WorkspaceId,
        name: impl Into<String> + std::fmt::Debug,
        attach_sandbox: Option<(SandboxId, SandboxAgentMode)>,
    ) -> Result<Agent, AgentError> {
        sub.can(AuthVerb::Create, AuthResource::Agent(workspace_id, None))?;
        Audit::record_action_if_unset("agent.create_agent");
        Audit::record_workspace_id(workspace_id);
        let workspace_name = self.resolve_workspace_name(workspace_id).await?;
        let id = AgentId::new();
        Audit::record_agent_id(id);
        let mut op = self.repo.begin_op().await?;
        let agent = self
            .create_in_op(
                &mut op,
                id,
                workspace_id,
                AgentRole::Agent,
                name,
                attach_sandbox,
                &workspace_name,
                None,
                None,
            )
            .await?;
        op.commit().await?;
        Ok(agent)
    }

    /// Caller commits the op. Stamping `(workflow_id, workflow_run_id)`
    /// is what excludes the agent from [`Self::list_for_workspace`].
    #[allow(clippy::too_many_arguments)]
    #[instrument(name = "domain.agent.create_for_workflow_run_in_op", skip(self, op))]
    pub async fn create_for_workflow_run_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        workspace_id: WorkspaceId,
        workflow_id: WorkflowDefinitionId,
        workflow_run_id: WorkflowRunId,
        name: impl Into<String> + std::fmt::Debug,
        attach_sandbox: Option<(SandboxId, SandboxAgentMode)>,
    ) -> Result<Agent, AgentError> {
        Audit::record_action_if_unset("agent.create_for_workflow_run");
        Audit::record_workspace_id(workspace_id);
        Audit::record_workflow_id(workflow_id);
        Audit::record_workflow_run_id(workflow_run_id);
        let workspace_name = self.resolve_workspace_name(workspace_id).await?;
        let id = AgentId::new();
        Audit::record_agent_id(id);
        self.create_in_op(
            op,
            id,
            workspace_id,
            AgentRole::Agent,
            name,
            attach_sandbox,
            &workspace_name,
            Some(workflow_id),
            Some(workflow_run_id),
        )
        .await
    }

    async fn resolve_workspace_name(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<String, AgentError> {
        let query = es_entity::PaginatedQueryArgs {
            first: 1,
            after: None,
        };
        let result = self
            .repo
            .list_for_workspace_id_by_created_at(
                workspace_id,
                query,
                es_entity::ListDirection::Ascending,
            )
            .await?;
        result
            .entities
            .into_iter()
            .next()
            .map(|a| a.workspace_name)
            .ok_or(AgentError::NoLeadAgent(workspace_id))
    }

    #[instrument(name = "domain.agent.create_workspace_lead_in_op", skip(self, op))]
    pub async fn create_workspace_lead_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        id: AgentId,
        workspace_id: WorkspaceId,
        name: impl Into<String> + std::fmt::Debug,
        workspace_name: &str,
    ) -> Result<Agent, AgentError> {
        Audit::record_workspace_id(workspace_id);
        Audit::record_agent_id(id);
        self.create_in_op(
            op,
            id,
            workspace_id,
            AgentRole::WorkspaceLead,
            name,
            None,
            workspace_name,
            None,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    #[instrument(name = "domain.agent.create_in_op", skip(self, op))]
    async fn create_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        id: AgentId,
        workspace_id: WorkspaceId,
        agent_role: AgentRole,
        name: impl Into<String> + std::fmt::Debug,
        attach_sandbox: Option<(SandboxId, SandboxAgentMode)>,
        workspace_name: &str,
        workflow_id: Option<WorkflowDefinitionId>,
        workflow_run_id: Option<WorkflowRunId>,
    ) -> Result<Agent, AgentError> {
        let role_config = self
            .config
            .builtin_roles
            .get(&agent_role)
            .ok_or(AgentError::RoleNotConfigured(agent_role))?
            .clone();

        let model_defaults = self
            .config
            .models
            .get(&role_config.model)
            .ok_or_else(|| AgentError::ModelNotConfigured(role_config.model.clone()))?;

        let authz_scopes = default_authz_scopes(agent_role, workspace_id);

        let mut new_agent_builder = NewAgent::builder();
        new_agent_builder
            .id(id)
            .workspace_id(workspace_id)
            .agent_role(agent_role)
            .name(name)
            .authz_scopes(authz_scopes)
            .workspace_name(workspace_name);
        if let Some(wf_id) = workflow_id {
            new_agent_builder.workflow_id(wf_id);
        }
        if let Some(run_id) = workflow_run_id {
            new_agent_builder.workflow_run_id(run_id);
        }
        let new_agent = new_agent_builder.build().expect("NewAgent build");

        let mut agent = self.repo.create_in_op(op, new_agent).await?;

        let agent_subject = agent.auth_subject();
        let tool_defs: Vec<session::message::ToolDefinition> = self
            .toolsets
            .top_level_tools(&agent_subject)
            .map(|t| session::message::ToolDefinition::from(llm::prompt::Tool::from(t.as_ref())))
            .collect();
        let mut system_blocks = system_prompt::system_blocks_for_role(
            agent_role,
            &self.toolsets,
            &agent_subject,
            &agent.workspace_name,
        );

        if let Some(notes) = &self.notes {
            if let Ok(Some(pinned_content)) = notes.pinned_context_for_workspace(workspace_id).await
            {
                system_blocks.push(session::message::SystemBlock::Notes {
                    text: pinned_content,
                });
            }
        }

        // Apply attach to the entity first so the initial skills block can
        // reflect the attached sandbox's exported skills.
        let initial_sandbox_id = if let Some((sandbox_id, mode)) = attach_sandbox {
            // Rejects WorkspaceLead before the sandbox round-trip.
            if agent.sandbox_attached(sandbox_id, mode)?.did_execute() {
                self.repo.update_in_op(op, &mut agent).await?;
            }
            Some(sandbox_id)
        } else {
            None
        };

        if let Ok(Some(skills_content)) = self
            .skills
            .skills_context_for_agent(workspace_id, initial_sandbox_id)
            .await
        {
            system_blocks.push(session::message::SystemBlock::Skills {
                text: skills_content,
            });
        }

        let session_model_defaults = ModelDefaults {
            model: role_config.model,
            ..model_defaults.clone()
        };

        self.sessions
            .create_in_op(
                op,
                agent.id,
                session_model_defaults,
                role_config.compaction.clone(),
                system_blocks,
                tool_defs,
            )
            .await?;

        if let Some((sandbox_id, mode)) = attach_sandbox {
            let sandbox = self
                .sandboxes
                .attach_to_agent_in_op(op, workspace_id, sandbox_id, agent.id, mode)
                .await?;

            self.sessions
                .sandbox_notification_in_op(
                    op,
                    agent.id,
                    sandbox.name,
                    session::message::SandboxOperation::Attach {
                        mode: format!("{mode:?}").to_lowercase(),
                        mount_path: sandbox.mount_path,
                    },
                )
                .await?;
        }

        Ok(agent)
    }

    #[instrument(name = "domain.agent.find_by_id", skip(self, sub))]
    pub async fn find_by_id(
        &self,
        sub: &AuthSubject,
        id: impl Into<AgentId> + std::fmt::Debug,
    ) -> Result<Agent, AgentError> {
        let agent = self.repo.find_by_id(id.into()).await?;
        sub.can(
            AuthVerb::Read,
            AuthResource::Agent(agent.workspace_id, Some(agent.id)),
        )?;
        Audit::record_action_if_unset("agent.find_by_id");
        Audit::record_workspace_id(agent.workspace_id);
        Audit::record_agent_id(agent.id);
        Ok(agent)
    }

    /// Workflow-spawned agents are filtered out; see
    /// [`Self::list_for_workflow_run`] for those.
    #[instrument(name = "domain.agent.list_for_workspace", skip(self, sub))]
    pub async fn list_for_workspace(
        &self,
        sub: &AuthSubject,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<Agent>, AgentError> {
        sub.can(AuthVerb::Read, AuthResource::Agent(workspace_id, None))?;
        Audit::record_action_if_unset("agent.list_for_workspace");
        Audit::record_workspace_id(workspace_id);
        let query = es_entity::PaginatedQueryArgs {
            first: 100,
            after: None,
        };
        let result = self
            .repo
            .list_for_workspace_id_by_created_at(
                workspace_id,
                query,
                es_entity::ListDirection::Descending,
            )
            .await?;
        // Covered by the `idx_agents_workspace_id_user_owned` partial index.
        Ok(result
            .entities
            .into_iter()
            .filter(|a| a.workflow_id.is_none())
            .collect())
    }

    #[instrument(name = "domain.agent.list_for_workflow_run", skip(self, sub))]
    pub async fn list_for_workflow_run(
        &self,
        sub: &AuthSubject,
        workspace_id: WorkspaceId,
        run_id: WorkflowRunId,
    ) -> Result<Vec<Agent>, AgentError> {
        sub.can(AuthVerb::Read, AuthResource::Agent(workspace_id, None))?;
        Audit::record_action_if_unset("agent.list_for_workflow_run");
        Audit::record_workspace_id(workspace_id);
        let query = es_entity::PaginatedQueryArgs {
            first: 100,
            after: None,
        };
        let result = self
            .repo
            .list_for_workflow_run_id_by_created_at(
                Some(run_id),
                query,
                es_entity::ListDirection::Ascending,
            )
            .await?;
        Ok(result.entities)
    }

    #[instrument(name = "domain.agent.list_for_workspace_in_op", skip(self, op))]
    pub(crate) async fn list_for_workspace_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<Agent>, AgentError> {
        Audit::record_workspace_id(workspace_id);
        let query = es_entity::PaginatedQueryArgs {
            first: 100,
            after: None,
        };
        let result = self
            .repo
            .list_for_workspace_id_by_created_at_in_op(
                &mut *op,
                workspace_id,
                query,
                es_entity::ListDirection::Descending,
            )
            .await?;
        Ok(result.entities)
    }

    #[instrument(name = "domain.agent.delete_in_op", skip(self, op))]
    pub async fn delete_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        id: impl Into<AgentId> + std::fmt::Debug,
    ) -> Result<(), AgentError> {
        let id = id.into();
        let agent = self.repo.find_by_id_in_op(&mut *op, id).await?;
        Audit::record_workspace_id(agent.workspace_id);
        Audit::record_agent_id(id);
        // Cascade soft-delete sessions before agent (mirrors workspace → agents).
        self.sessions.delete_for_agent_in_op(op, id).await?;
        self.repo.delete_in_op(op, agent).await?;
        Ok(())
    }

    /// Re-attach with a different mode: downgrade unconditional; upgrade to
    /// Write only if no other agent holds Write. The matching
    /// `SandboxRead`/`SandboxWrite` scope replaces any stale opposite-mode
    /// scope on the agent.
    #[instrument(name = "domain.agent.attach_sandbox", skip(self, subject))]
    pub async fn attach_sandbox(
        &self,
        subject: &AuthSubject,
        agent_id: AgentId,
        sandbox_id: SandboxId,
        mode: SandboxAgentMode,
    ) -> Result<Agent, AgentError> {
        let agent = self.repo.find_by_id(agent_id).await?;
        let workspace_id = agent.workspace_id;
        subject.can(
            AuthVerb::Update,
            AuthResource::Agent(workspace_id, Some(agent.id)),
        )?;
        Audit::record_action_if_unset("agent.attach_sandbox");
        Audit::record_workspace_id(workspace_id);
        Audit::record_agent_id(agent_id);

        let mut op = self.repo.begin_op().await?;

        // Agent side first: `sandbox_attached` enforces entity invariants (lead
        // can't attach; at most one sandbox per agent), short-circuiting before
        // the sandbox round-trip.
        let mut agent = self.repo.find_by_id_in_op(&mut op, agent_id).await?;
        if agent.sandbox_attached(sandbox_id, mode)?.did_execute() {
            self.repo.update_in_op(&mut op, &mut agent).await?;
        }

        let sandbox = self
            .sandboxes
            .attach_to_agent_in_op(&mut op, workspace_id, sandbox_id, agent_id, mode)
            .await?;

        self.sessions
            .sandbox_notification_in_op(
                &mut op,
                agent_id,
                sandbox.name,
                session::message::SandboxOperation::Attach {
                    mode: format!("{mode:?}").to_lowercase(),
                    mount_path: sandbox.mount_path,
                },
            )
            .await?;

        self.refresh_skills_block_in_op(&mut op, agent_id, workspace_id, Some(sandbox_id))
            .await;

        op.commit().await?;

        Ok(agent)
    }

    /// Idempotent at both entity-attach-list and agent-scope layers.
    #[instrument(name = "domain.agent.detach_sandbox", skip(self, subject))]
    pub async fn detach_sandbox(
        &self,
        subject: &AuthSubject,
        agent_id: AgentId,
        sandbox_id: SandboxId,
    ) -> Result<Agent, AgentError> {
        let agent = self.repo.find_by_id(agent_id).await?;
        subject.can(
            AuthVerb::Update,
            AuthResource::Agent(agent.workspace_id, Some(agent.id)),
        )?;
        Audit::record_action_if_unset("agent.detach_sandbox");
        Audit::record_workspace_id(agent.workspace_id);
        Audit::record_agent_id(agent_id);

        let mut op = self.repo.begin_op().await?;

        let mut agent = self.repo.find_by_id_in_op(&mut op, agent_id).await?;
        if agent.sandbox_detached(sandbox_id).did_execute() {
            self.repo.update_in_op(&mut op, &mut agent).await?;
        }

        let sandbox = self
            .sandboxes
            .detach_from_agent_in_op(&mut op, sandbox_id, agent_id)
            .await?;

        self.sessions
            .sandbox_notification_in_op(
                &mut op,
                agent_id,
                sandbox.name,
                session::message::SandboxOperation::Detach,
            )
            .await?;

        self.refresh_skills_block_in_op(&mut op, agent_id, agent.workspace_id, None)
            .await;

        op.commit().await?;

        Ok(agent)
    }

    /// Recomputes the `Skills` system block for `agent_id` scoped to
    /// `sandbox_id` and pushes it. Idempotent: when the block content matches
    /// the latest persisted skills block, no event is emitted. Errors from
    /// the skills service or session push are logged and swallowed — sandbox
    /// attach/detach must not fail because of a transient skills-context
    /// problem.
    async fn refresh_skills_block_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        agent_id: AgentId,
        workspace_id: WorkspaceId,
        sandbox_id: Option<SandboxId>,
    ) {
        let skills_text = match self
            .skills
            .skills_context_for_agent(workspace_id, sandbox_id)
            .await
        {
            Ok(Some(text)) => text,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(error = %e, "skills_context_for_agent failed during refresh");
                return;
            }
        };
        if let Err(e) = self
            .sessions
            .push_system_block_in_op(
                op,
                agent_id,
                session::message::SystemBlock::Skills { text: skills_text },
            )
            .await
        {
            tracing::warn!(error = %e, "push_system_block_in_op failed during skills refresh");
        }
    }

    #[instrument(name = "domain.agent.chat_history", skip(self, sub))]
    pub async fn chat_history(
        &self,
        sub: &AuthSubject,
        agent_id: AgentId,
        last_n: usize,
    ) -> Result<Vec<session::history::ChatHistoryMessage>, AgentError> {
        let agent = self.repo.find_by_id(agent_id).await?;
        sub.can(
            AuthVerb::Read,
            AuthResource::Agent(agent.workspace_id, Some(agent.id)),
        )?;
        Audit::record_action_if_unset("agent.chat_history");
        Audit::record_workspace_id(agent.workspace_id);
        Audit::record_agent_id(agent_id);
        Ok(self.sessions.chat_history(agent_id, last_n).await?)
    }

    #[instrument(name = "domain.agent.thread_infos", skip(self, sub))]
    pub async fn thread_infos(
        &self,
        sub: &AuthSubject,
        agent_id: AgentId,
    ) -> Result<Vec<session::history::SessionThreadInfo>, AgentError> {
        let agent = self.repo.find_by_id(agent_id).await?;
        sub.can(
            AuthVerb::Read,
            AuthResource::Agent(agent.workspace_id, Some(agent.id)),
        )?;
        Audit::record_action_if_unset("agent.thread_infos");
        Audit::record_workspace_id(agent.workspace_id);
        Audit::record_agent_id(agent_id);
        Ok(self.sessions.thread_infos(agent_id).await?)
    }

    #[instrument(name = "domain.agent.thread_messages", skip(self, sub))]
    pub async fn thread_messages(
        &self,
        sub: &AuthSubject,
        agent_id: AgentId,
        thread_id: session::SessionThreadId,
    ) -> Result<Vec<session::history::ThreadMessage>, AgentError> {
        let agent = self.repo.find_by_id(agent_id).await?;
        sub.can(
            AuthVerb::Read,
            AuthResource::Agent(agent.workspace_id, Some(agent.id)),
        )?;
        Audit::record_action_if_unset("agent.thread_messages");
        Audit::record_workspace_id(agent.workspace_id);
        Audit::record_agent_id(agent_id);
        Ok(self.sessions.thread_messages(agent_id, thread_id).await?)
    }

    #[instrument(name = "domain.agent.thread_system_view", skip(self, sub))]
    pub async fn thread_system_view(
        &self,
        sub: &AuthSubject,
        agent_id: AgentId,
        thread_id: session::SessionThreadId,
    ) -> Result<session::history::ThreadSystemView, AgentError> {
        let agent = self.repo.find_by_id(agent_id).await?;
        sub.can(
            AuthVerb::Read,
            AuthResource::Agent(agent.workspace_id, Some(agent.id)),
        )?;
        Audit::record_action_if_unset("agent.thread_system_view");
        Audit::record_workspace_id(agent.workspace_id);
        Audit::record_agent_id(agent_id);
        Ok(self
            .sessions
            .thread_system_view(agent_id, thread_id)
            .await?)
    }

    /// Export a thread as Pi-compatible JSONL (v3). `None` exports main thread.
    #[instrument(name = "domain.agent.export_thread", skip(self, sub))]
    pub async fn export_thread(
        &self,
        sub: &AuthSubject,
        agent_id: AgentId,
        thread_id: Option<session::SessionThreadId>,
    ) -> Result<String, AgentError> {
        let agent = self.repo.find_by_id(agent_id).await?;
        sub.can(
            AuthVerb::Read,
            AuthResource::Agent(agent.workspace_id, Some(agent.id)),
        )?;
        Audit::record_action_if_unset("agent.export_thread");
        Audit::record_workspace_id(agent.workspace_id);
        Audit::record_agent_id(agent_id);
        let target = thread_id
            .map(session::TargetThread::Id)
            .unwrap_or(session::TargetThread::Main);
        let exportable = self.sessions.export_thread(agent_id, target).await?;
        Ok(pi_export::export_to_jsonl(&exportable))
    }

    #[instrument(name = "domain.agent.send_message", skip(self, prompt))]
    pub async fn send_message(
        &self,
        subject: AuthSubject,
        id: AgentId,
        prompt: String,
    ) -> Result<tokio::sync::mpsc::Receiver<ChatOutputEvent>, AgentError> {
        let agent = self.repo.find_by_id(id).await?;

        // Agents may only message peers in their own workspace; anonymous rejected.
        match &subject {
            AuthSubject::User(_) | AuthSubject::ExportedAgent(_, _, _) => {}
            AuthSubject::Agent(ws, _, _) | AuthSubject::AgentOnBehalfOfUser(_, ws, _, _)
                if *ws == agent.workspace_id => {}
            _ => return Err(AgentError::Unauthorized),
        }

        Audit::record_action_if_unset("agent.send_message");
        Audit::record_workspace_id(agent.workspace_id);
        Audit::record_agent_id(id);
        if let Some(wf) = agent.workflow_id {
            Audit::record_workflow_id(wf);
        }
        if let Some(run) = agent.workflow_run_id {
            Audit::record_workflow_run_id(run);
        }

        let source = subject.to_message_source();
        let (tx, rx) = tokio::sync::mpsc::channel::<ChatOutputEvent>(64);

        // Attribute tool calls to the originating user if available.
        let agent_subject = match subject.originating_user_id() {
            Some(user_id) => agent.auth_subject_for_user(user_id),
            None => agent.auth_subject(),
        };

        // Pass blocks into `add_user_input` so diff + user-input share a single
        // repo round-trip; `next_prompt` later spawns a new thread if changed.
        let dynamic_blocks = self
            .cached_dynamic_blocks(agent.workspace_id, agent.attached_sandbox_id())
            .await;
        let mut proposed_system_blocks = system_prompt::system_blocks_for_role(
            agent.agent_role,
            &self.toolsets,
            &agent_subject,
            &agent.workspace_name,
        );
        proposed_system_blocks.extend(dynamic_blocks);

        let session_response = self
            .sessions
            .add_user_input(
                id,
                session::TargetThread::Main,
                source,
                prompt.clone(),
                proposed_system_blocks,
            )
            .await?;

        match session_response {
            session::AgentSessionResponse::PromptPending { .. } => {}
            other => {
                // Thread stuck in assistant/tool-use turn; emit a Service event
                // so the caller doesn't see an empty stream.
                let msg = match other {
                    session::AgentSessionResponse::AwaitingToolUsageComplete => {
                        "Message queued — session is waiting for tool results that will never arrive. Consider creating a new workspace."
                    }
                    session::AgentSessionResponse::AwaitingAssistantResponse => {
                        "Message queued — session is waiting for an assistant response. Try again shortly."
                    }
                    _ => "Message queued — session is busy.",
                };
                let tx_clone = tx.clone();
                tokio::spawn(async move {
                    let _ = tx_clone
                        .send(ChatOutputEvent::Service {
                            message: msg.to_string(),
                        })
                        .await;
                });
                return Ok(rx);
            }
        }

        let _ = tx
            .send(ChatOutputEvent::UserMessage {
                source,
                text: prompt,
            })
            .await;

        let prompt_state = self
            .sessions
            .next_prompt(id, session::TargetThread::Main)
            .await?;

        let model_name = prompt_state.model.clone();
        let (request, response_rx) = llm::PromptRequest::new(prompt_state);
        self.prompt_requests
            .send(request)
            .await
            .map_err(|_| AgentError::PromptRequestChannelClosed)?;

        let sessions = self.sessions.clone();
        let toolsets = self.toolsets.clone();
        let prompt_requests = self.prompt_requests.clone();
        tokio::spawn(async move {
            let mut next = response_rx.await;
            let mut turn: u32 = 0;
            let mut input_tokens: u32 = 0;
            let mut output_tokens: u32 = 0;
            let mut current_model = model_name;
            loop {
                turn += 1;
                let result = match next {
                    Ok(Ok(r)) => r,
                    Ok(Err(e)) => {
                        let msg = e.to_string();
                        let _ = sessions
                            .assistant_response_failed(id, current_model.clone(), msg.clone())
                            .await;
                        let _ = tx.send(ChatOutputEvent::Error { message: msg }).await;
                        return;
                    }
                    Err(_) => {
                        let msg = "prompt response channel closed".to_string();
                        let _ = sessions
                            .assistant_response_failed(id, current_model.clone(), msg.clone())
                            .await;
                        let _ = tx.send(ChatOutputEvent::Error { message: msg }).await;
                        return;
                    }
                };

                let (response, streamed) = match result {
                    llm::PromptResult::Stream(handle) => match consume_stream(handle, &tx).await {
                        Ok(resp) => (resp, true),
                        Err(msg) => {
                            let _ = sessions
                                .assistant_response_failed(id, current_model.clone(), msg)
                                .await;
                            return;
                        }
                    },
                    llm::PromptResult::Complete(response) => (response, false),
                };

                input_tokens += response.usage.input_tokens;
                output_tokens += response.usage.output_tokens;

                let session_response = match sessions
                    .assistant_response_received(id, response.clone(), current_model.clone())
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = tx
                            .send(ChatOutputEvent::Error {
                                message: e.to_string(),
                            })
                            .await;
                        return;
                    }
                };

                if !streamed {
                    forward_response(response, &tx).await;
                }

                let next_prompt = match session_response {
                    session::AgentSessionResponse::Done => break,
                    session::AgentSessionResponse::ToolUseRequest(tool_uses) => {
                        let tool_calls: Vec<llm::RequestToolUse> = tool_uses
                            .into_iter()
                            .map(|tu| llm::RequestToolUse {
                                id: tu.id,
                                name: tu.name,
                                input: tu.input,
                            })
                            .collect();
                        let results =
                            fan_out_tool_calls(&toolsets, &agent_subject, tool_calls, &tx).await;

                        if let Err(e) = sessions.add_tool_results(id, results).await {
                            let _ = tx
                                .send(ChatOutputEvent::Error {
                                    message: e.to_string(),
                                })
                                .await;
                            return;
                        }

                        match sessions.next_prompt(id, session::TargetThread::Main).await {
                            Ok(p) => p,
                            Err(e) => {
                                let _ = tx
                                    .send(ChatOutputEvent::Error {
                                        message: e.to_string(),
                                    })
                                    .await;
                                return;
                            }
                        }
                    }
                    session::AgentSessionResponse::PromptPending { target } => {
                        match sessions.next_prompt(id, target).await {
                            Ok(p) => p,
                            Err(e) => {
                                let _ = tx
                                    .send(ChatOutputEvent::Error {
                                        message: e.to_string(),
                                    })
                                    .await;
                                return;
                            }
                        }
                    }
                    _ => break,
                };

                current_model = next_prompt.model.clone();
                let (request, rx_next) = llm::PromptRequest::new(next_prompt);
                if prompt_requests.send(request).await.is_err() {
                    let _ = tx
                        .send(ChatOutputEvent::Error {
                            message: "prompt request channel closed".to_string(),
                        })
                        .await;
                    return;
                }
                next = rx_next.await;
            }

            let _ = tx
                .send(ChatOutputEvent::AssistantDone {
                    turns: turn,
                    input_tokens,
                    output_tokens,
                    duration_ms: None,
                    cost_usd: None,
                })
                .await;
        });

        Ok(rx)
    }
}

/// Drain a streaming LLM response, forwarding deltas and accumulating the
/// final response. Returns `Err(msg)` on stream error; the caller is
/// responsible for forwarding the error to the UI and the session.
async fn consume_stream(
    handle: llm::StreamHandle,
    tx: &tokio::sync::mpsc::Sender<ChatOutputEvent>,
) -> Result<llm::PromptResponse, String> {
    let mut acc = llm::stream::StreamAccumulator::new();
    let mut rx = handle.rx;
    while let Some(event) = rx.recv().await {
        match event {
            Ok(delta) => {
                if let Some(chat_event) = delta_to_chat_event(&delta) {
                    let _ = tx.send(chat_event).await;
                }
                acc.process(&delta);
            }
            Err(e) => {
                let msg = e.to_string();
                let _ = tx
                    .send(ChatOutputEvent::Error {
                        message: msg.clone(),
                    })
                    .await;
                return Err(msg);
            }
        }
    }
    Ok(acc.finish())
}

fn delta_to_chat_event(delta: &llm::stream::StreamDelta) -> Option<ChatOutputEvent> {
    use llm::stream::StreamDelta;
    match delta {
        StreamDelta::TextDelta { text } => {
            Some(ChatOutputEvent::AssistantTextDelta { text: text.clone() })
        }
        StreamDelta::ThinkingDelta { text } => {
            Some(ChatOutputEvent::ThinkingDelta { text: text.clone() })
        }
        StreamDelta::ToolCallStart { name, .. } => {
            Some(ChatOutputEvent::ToolCallStart { name: name.clone() })
        }
        StreamDelta::ToolCallDelta { partial_json, .. } => {
            Some(ChatOutputEvent::ToolCallInputDelta {
                partial_json: partial_json.clone(),
            })
        }
        _ => None,
    }
}

async fn fan_out_tool_calls(
    toolsets: &Arc<ToolSets>,
    subject: &AuthSubject,
    calls: Vec<llm::RequestToolUse>,
    tx: &tokio::sync::mpsc::Sender<ChatOutputEvent>,
) -> Vec<llm::ToolUseResult> {
    let dispatches = calls.into_iter().map(|tu| {
        let toolsets = toolsets.clone();
        let subject = subject.clone();
        async move {
            let id = tu.id.clone();
            let name = tu.name.clone();
            let result = match toolsets
                .call_top_level_tool(&subject, &name, tu.input.as_object().cloned())
                .await
            {
                Ok(r) => llm::ToolUseResult {
                    tool_use_id: id,
                    content: call_result_to_text(&r),
                    is_error: r.is_error.unwrap_or(false),
                },
                Err(e) => llm::ToolUseResult {
                    tool_use_id: id,
                    content: e.to_string(),
                    is_error: true,
                },
            };
            (name, result)
        }
    });

    let outcomes = futures::future::join_all(dispatches).await;
    let mut results = Vec::with_capacity(outcomes.len());
    for (name, result) in outcomes {
        const MAX_CONTENT_LEN: usize = 4096;
        let content = if result.content.is_empty() {
            None
        } else if result.content.len() <= MAX_CONTENT_LEN {
            Some(result.content.clone())
        } else {
            let truncated: String = result.content.chars().take(MAX_CONTENT_LEN).collect();
            Some(truncated + "…")
        };
        let _ = tx
            .send(ChatOutputEvent::ToolResult {
                name,
                is_error: result.is_error,
                content,
            })
            .await;
        results.push(result);
    }
    results
}

fn call_result_to_text(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn forward_response(
    response: llm::PromptResponse,
    tx: &tokio::sync::mpsc::Sender<ChatOutputEvent>,
) {
    for block in response.content {
        match block {
            llm::prompt::AssistantBlock::Text { text, .. } => {
                let _ = tx.send(ChatOutputEvent::AssistantText { text }).await;
            }
            llm::prompt::AssistantBlock::ToolUse { name, input, .. } => {
                let _ = tx.send(ChatOutputEvent::ToolCallStart { name }).await;
                let _ = tx
                    .send(ChatOutputEvent::ToolCallInputDelta {
                        partial_json: input.to_string(),
                    })
                    .await;
            }
            llm::prompt::AssistantBlock::Thinking { text, .. } => {
                let _ = tx.send(ChatOutputEvent::Thinking { text }).await;
            }
        }
    }
}
