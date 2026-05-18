pub mod config;
mod entity;
pub mod error;
mod pi_export;
pub mod repo;
pub mod scope;
pub mod session;
mod system_prompt;

pub use scope::AgentScope;

use std::sync::Arc;

use crate::audit::Audit;
use crate::library::SpaceMounts;
use crate::note::Notes;
use crate::skill::Skills;
use crate::toolset::ToolSets;

/// Lead → `ProjectAdmin`; Agent → `ProjectMember`; WorkflowStepAgent →
/// `ProjectMember` + `WorkflowStepAgent` marker (gates submit_output's
/// visibility). Sandbox scopes are added later via
/// [`Agent::sandbox_attached`].
fn default_authz_scopes(role: AgentRole, project_id: ProjectId) -> Vec<AuthScope> {
    match role {
        AgentRole::ProjectLead => vec![AuthScope::ProjectAdmin(project_id)],
        AgentRole::Agent => vec![AuthScope::ProjectMember(project_id)],
        AgentRole::WorkflowStepAgent => vec![
            AuthScope::ProjectMember(project_id),
            AuthScope::WorkflowStepAgent,
        ],
    }
}

use tracing::instrument;

use crate::primitives::{
    AgentId, AuthResource, AuthScope, AuthSubject, AuthVerb, ChatOutputEvent, ContextGeneration,
    ProjectId, SandboxId, WorkflowDefinitionId, WorkflowRunId,
};
use crate::sandbox::{SandboxAgentMode, Sandboxes};
pub use config::{AgentsConfig, ModelChain, ModelDefaults, RoleConfig};
pub use entity::*;
pub use error::AgentError;
use repo::AgentRepo;
use session::Sessions;

/// Snapshot of dynamic system blocks (notes + skills + spaces) for one
/// agent, keyed by `ContextGeneration` at fetch time. Skips DB round-trips
/// on the hot path. Per-agent rather than per-project so two agents in the
/// same project with different sandbox attachments don't share an entry.
#[derive(Clone)]
struct CachedAgentContext {
    generation: u64,
    notes_block: Option<session::message::SystemBlock>,
    skills_block: Option<session::message::SystemBlock>,
    spaces_block: Option<session::message::SystemBlock>,
}

impl CachedAgentContext {
    fn to_blocks(&self) -> Vec<session::message::SystemBlock> {
        let mut out = Vec::with_capacity(3);
        if let Some(b) = &self.notes_block {
            out.push(b.clone());
        }
        if let Some(b) = &self.skills_block {
            out.push(b.clone());
        }
        if let Some(b) = &self.spaces_block {
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
    /// Read-only facade for the project↔space mount relationship.
    /// Used to derive `AgentScope.mounted_space_ids` and to render the
    /// `<spaces>` system block. Holds its own `ProjectRepo` + library
    /// `AuthedSpaces` internally — no upward dep on the `Projects`
    /// service, so the previous `OnceLock<Projects>` cycle is gone.
    space_mounts: Arc<SpaceMounts>,
    config: AgentsConfig,
    toolsets: Arc<ToolSets>,
    prompt_requests: llm::PromptRequestChannel,
    context_generation: ContextGeneration,
    context_cache: Arc<std::sync::RwLock<std::collections::HashMap<AgentId, CachedAgentContext>>>,
}

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
        space_mounts: Arc<SpaceMounts>,
    ) -> Self {
        Self {
            repo: AgentRepo::new(pool),
            sessions: Sessions::new(pool, config.clone()),
            sandboxes,
            skills,
            notes,
            space_mounts,
            config,
            toolsets,
            prompt_requests,
            context_generation,
            context_cache: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
        }
    }

    /// Hot-path lookup of dynamic system blocks for a specific agent. Reads
    /// `ContextGeneration` atomically; only hits DB when the generation has
    /// bumped. Per-agent cache so attached sandbox (and future per-agent
    /// scope dimensions) doesn't bleed across agents in the same project.
    async fn cached_dynamic_blocks(&self, agent: &Agent) -> Vec<session::message::SystemBlock> {
        let current_gen = self.context_generation.current();

        {
            let cache = self.context_cache.read().expect("context_cache poisoned");
            if let Some(cached) = cache.get(&agent.id) {
                if cached.generation == current_gen {
                    return cached.to_blocks();
                }
            }
        }

        let project_id = agent.project_id;

        // Resolve scope once; both notes and skills blocks consume it
        // (so a project-mounted space's auto-pinned notes line up with
        // its skills in the same render pass).
        let scope_result = scope::AgentScope::for_agent(agent, &self.space_mounts).await;

        let notes_block = match (&self.notes, &scope_result) {
            (Some(notes), Ok(scope)) => notes
                .pinned_context_for_scope(scope)
                .await
                .ok()
                .flatten()
                .map(|text| session::message::SystemBlock::Notes { text }),
            (Some(notes), Err(e)) => {
                tracing::warn!(error = %e, "AgentScope::for_agent failed; rendering notes block without space tier");
                // Fall back to a project-only scope so project-pinned
                // notes still render. Space auto-pin is degraded only
                // when SpaceMounts is unavailable.
                let project_only = scope::AgentScope {
                    project_id,
                    attached_sandbox_id: agent.attached_sandbox_id(),
                    mounted_space_ids: Vec::new(),
                };
                notes
                    .pinned_context_for_scope(&project_only)
                    .await
                    .ok()
                    .flatten()
                    .map(|text| session::message::SystemBlock::Notes { text })
            }
            (None, _) => None,
        };
        let skills_block = match &scope_result {
            Ok(scope) => self
                .skills
                .skills_context_for_scope(scope)
                .await
                .ok()
                .flatten()
                .map(|text| session::message::SystemBlock::Skills { text }),
            Err(e) => {
                tracing::warn!(error = %e, "AgentScope::for_agent failed; rendering skills block without space tier");
                self.skills
                    .skills_context_for_agent(project_id, agent.attached_sandbox_id())
                    .await
                    .ok()
                    .flatten()
                    .map(|text| session::message::SystemBlock::Skills { text })
            }
        };
        let spaces_block = self
            .space_mounts
            .spaces_block_for_project(project_id)
            .await
            .ok()
            .flatten()
            .map(|text| session::message::SystemBlock::Spaces { text });

        let entry = CachedAgentContext {
            generation: current_gen,
            notes_block: notes_block.clone(),
            skills_block: skills_block.clone(),
            spaces_block: spaces_block.clone(),
        };
        let result = entry.to_blocks();
        if let Ok(mut cache) = self.context_cache.write() {
            cache.insert(agent.id, entry);
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

    #[instrument(name = "domain.agent.create_project_lead", skip(self, sub))]
    pub async fn create_project_lead(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
        name: impl Into<String> + std::fmt::Debug,
        project_name: &str,
    ) -> Result<Agent, AgentError> {
        sub.can(AuthVerb::Create, AuthResource::Agent(project_id, None))?;
        Audit::record_action_if_unset("agent.create_project_lead");
        Audit::record_project_id(project_id);
        let id = AgentId::new();
        Audit::record_agent_id(id);
        let mut op = self.repo.begin_op().await?;
        let agent = self
            .create_in_op(
                &mut op,
                id,
                project_id,
                AgentRole::ProjectLead,
                name,
                None,
                project_name,
                None,
                None,
                None,
                None,
            )
            .await?;
        op.commit().await?;
        Ok(agent)
    }

    /// Project name is resolved from the existing lead agent.
    #[instrument(name = "domain.agent.create_agent", skip(self, sub))]
    pub async fn create_agent(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
        name: impl Into<String> + std::fmt::Debug,
        attach_sandbox: Option<(SandboxId, SandboxAgentMode)>,
        chain_override: Option<llm::ModelChain>,
    ) -> Result<Agent, AgentError> {
        sub.can(AuthVerb::Create, AuthResource::Agent(project_id, None))?;
        Audit::record_action_if_unset("agent.create_agent");
        Audit::record_project_id(project_id);
        let project_name = self.resolve_project_name(project_id).await?;
        let id = AgentId::new();
        Audit::record_agent_id(id);
        let mut op = self.repo.begin_op().await?;
        let agent = self
            .create_in_op(
                &mut op,
                id,
                project_id,
                AgentRole::Agent,
                name,
                attach_sandbox,
                &project_name,
                None,
                None,
                chain_override,
                None,
            )
            .await?;
        op.commit().await?;
        Ok(agent)
    }

    /// Caller commits the op. Stamping `(workflow_id, workflow_run_id)`
    /// is what excludes the agent from [`Self::list_for_project`].
    /// `output_schema` is persisted on the agent entity; the
    /// `submit_output` tool reads it at call time to validate args.
    #[allow(clippy::too_many_arguments)]
    #[instrument(name = "domain.agent.create_for_workflow_run_in_op", skip(self, op))]
    pub async fn create_for_workflow_run_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        project_id: ProjectId,
        workflow_id: WorkflowDefinitionId,
        workflow_run_id: WorkflowRunId,
        name: impl Into<String> + std::fmt::Debug,
        attach_sandbox: Option<(SandboxId, SandboxAgentMode)>,
        chain_override: Option<llm::ModelChain>,
        output_schema: crate::workflow::OutputSchema,
    ) -> Result<Agent, AgentError> {
        Audit::record_action_if_unset("agent.create_for_workflow_run");
        Audit::record_project_id(project_id);
        Audit::record_workflow_id(workflow_id);
        Audit::record_workflow_run_id(workflow_run_id);
        let project_name = self.resolve_project_name(project_id).await?;
        let id = AgentId::new();
        Audit::record_agent_id(id);
        self.create_in_op(
            op,
            id,
            project_id,
            AgentRole::WorkflowStepAgent,
            name,
            attach_sandbox,
            &project_name,
            Some(workflow_id),
            Some(workflow_run_id),
            chain_override,
            Some(output_schema),
        )
        .await
    }

    async fn resolve_project_name(&self, project_id: ProjectId) -> Result<String, AgentError> {
        let query = es_entity::PaginatedQueryArgs {
            first: 1,
            after: None,
        };
        let result = self
            .repo
            .list_for_project_id_by_created_at(
                project_id,
                query,
                es_entity::ListDirection::Ascending,
            )
            .await?;
        result
            .entities
            .into_iter()
            .next()
            .map(|a| a.project_name)
            .ok_or(AgentError::NoLeadAgent(project_id))
    }

    #[instrument(name = "domain.agent.create_project_lead_in_op", skip(self, op))]
    pub async fn create_project_lead_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        id: AgentId,
        project_id: ProjectId,
        name: impl Into<String> + std::fmt::Debug,
        project_name: &str,
    ) -> Result<Agent, AgentError> {
        Audit::record_project_id(project_id);
        Audit::record_agent_id(id);
        self.create_in_op(
            op,
            id,
            project_id,
            AgentRole::ProjectLead,
            name,
            None,
            project_name,
            None,
            None,
            None,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    #[instrument(name = "domain.agent.create_in_op", skip(self, op, output_schema))]
    async fn create_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        id: AgentId,
        project_id: ProjectId,
        agent_role: AgentRole,
        name: impl Into<String> + std::fmt::Debug,
        attach_sandbox: Option<(SandboxId, SandboxAgentMode)>,
        project_name: &str,
        workflow_id: Option<WorkflowDefinitionId>,
        workflow_run_id: Option<WorkflowRunId>,
        chain_override: Option<llm::ModelChain>,
        output_schema: Option<crate::workflow::OutputSchema>,
    ) -> Result<Agent, AgentError> {
        let role_config = self
            .config
            .builtin_roles
            .get(&agent_role)
            .ok_or(AgentError::RoleNotConfigured(agent_role))?
            .clone();

        let authz_scopes = default_authz_scopes(agent_role, project_id);

        let mut new_agent_builder = NewAgent::builder();
        new_agent_builder
            .id(id)
            .project_id(project_id)
            .agent_role(agent_role)
            .name(name)
            .authz_scopes(authz_scopes)
            .project_name(project_name)
            .output_schema(output_schema.clone());
        if let Some(wf_id) = workflow_id {
            new_agent_builder.workflow_id(wf_id);
        }
        if let Some(run_id) = workflow_run_id {
            new_agent_builder.workflow_run_id(run_id);
        }
        let new_agent = new_agent_builder.build().expect("NewAgent build");

        let mut agent = self.repo.create_in_op(op, new_agent).await?;

        let agent_subject = agent.auth_subject();
        let tool_defs = self
            .toolsets
            .top_level_tool_defs(&agent_subject, output_schema.as_ref());
        let mut system_blocks = system_prompt::system_blocks_for_role(
            agent_role,
            &self.toolsets,
            &agent_subject,
            &agent.project_name,
            output_schema.as_ref(),
        );

        // Apply attach to the entity first so the initial skills block reflects
        // the attached sandbox's exported skills. Rejects ProjectLead before
        // the sandbox round-trip (`sandbox_attached` enforces it).
        let initial_sandbox_id = if let Some((sandbox_id, mode)) = attach_sandbox {
            if agent.sandbox_attached(sandbox_id, mode)?.did_execute() {
                self.repo.update_in_op(op, &mut agent).await?;
            }
            Some(sandbox_id)
        } else {
            None
        };

        // Resolve scope once; both notes (auto-pin from mounted spaces)
        // and skills consume it.
        let initial_scope = match scope::AgentScope::for_agent(&agent, &self.space_mounts).await {
            Ok(mut scope) => {
                scope.attached_sandbox_id = initial_sandbox_id;
                Some(scope)
            }
            Err(e) => {
                tracing::warn!(error = %e, "AgentScope::for_agent failed at create; rendering initial blocks without space tier");
                None
            }
        };

        if let Some(notes) = &self.notes {
            let pinned = match &initial_scope {
                Some(scope) => notes.pinned_context_for_scope(scope).await,
                None => {
                    let project_only = scope::AgentScope {
                        project_id,
                        attached_sandbox_id: initial_sandbox_id,
                        mounted_space_ids: Vec::new(),
                    };
                    notes.pinned_context_for_scope(&project_only).await
                }
            };
            if let Ok(Some(pinned_content)) = pinned {
                system_blocks.push(session::message::SystemBlock::Notes {
                    text: pinned_content,
                });
            }
        }

        let initial_skills = match &initial_scope {
            Some(scope) => self.skills.skills_context_for_scope(scope).await,
            None => {
                self.skills
                    .skills_context_for_agent(project_id, initial_sandbox_id)
                    .await
            }
        };
        if let Ok(Some(skills_content)) = initial_skills {
            system_blocks.push(session::message::SystemBlock::Skills {
                text: skills_content,
            });
        }

        if let Ok(Some(spaces_content)) =
            self.space_mounts.spaces_block_for_project(project_id).await
        {
            system_blocks.push(session::message::SystemBlock::Spaces {
                text: spaces_content,
            });
        }

        self.sessions
            .create_in_op(
                op,
                agent.id,
                agent_role,
                chain_override,
                role_config.compaction.clone(),
                system_blocks,
                tool_defs,
            )
            .await?;

        if let Some((sandbox_id, mode)) = attach_sandbox {
            let sandbox = self
                .sandboxes
                .attach_to_agent_in_op(op, project_id, sandbox_id, agent.id, mode)
                .await?;

            let (kind, scope) = sandbox.kind_and_scope();
            let push_policy = self.sandboxes.push_policy_text(&sandbox.mode);
            self.sessions
                .sandbox_notification_in_op(
                    op,
                    agent.id,
                    sandbox.name,
                    session::message::SandboxOperation::Attach {
                        agent_mode: format!("{mode:?}").to_lowercase(),
                        kind: kind.to_string(),
                        cwd: sandbox.cwd,
                        scope,
                        push_policy,
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
            AuthResource::Agent(agent.project_id, Some(agent.id)),
        )?;
        Audit::record_action_if_unset("agent.find_by_id");
        Audit::record_project_id(agent.project_id);
        Audit::record_agent_id(agent.id);
        Ok(agent)
    }

    #[instrument(name = "domain.agent.update_session_chain", skip(self, sub))]
    pub async fn update_session_chain(
        &self,
        sub: &AuthSubject,
        agent_id: AgentId,
        model_chain: Option<llm::ModelChain>,
    ) -> Result<session::AgentSession, AgentError> {
        let agent = self.repo.find_by_id(agent_id).await?;
        sub.can(
            AuthVerb::Update,
            AuthResource::Agent(agent.project_id, Some(agent.id)),
        )?;
        if agent.is_workflow_agent() {
            return Err(AgentError::WorkflowAgentChainImmutable);
        }
        Audit::record_action_if_unset("agent.update_session_chain");
        Audit::record_project_id(agent.project_id);
        Audit::record_agent_id(agent.id);

        let session = self
            .sessions
            .update_chain_for_agent(agent.agent_role, agent.id, model_chain)
            .await?;
        Ok(session)
    }

    /// Read-only accessor for an agent's persisted `output_schema`.
    /// Used by `SubmitOutputTool::call` to validate the model's args
    /// against the schema set at agent creation. No authz check —
    /// the caller is the agent reading its own persisted state.
    #[instrument(name = "domain.agent.output_schema_for_agent", skip(self))]
    pub async fn output_schema_for_agent(
        &self,
        agent_id: AgentId,
    ) -> Result<Option<crate::workflow::OutputSchema>, AgentError> {
        let agent = self.repo.find_by_id(agent_id).await?;
        Ok(agent.output_schema.clone())
    }

    /// Reads the agent's session for a persisted `OutputSubmitted`
    /// event. Workflow executor consumes this after
    /// `AssistantDone` to populate `StepResult.output`.
    #[instrument(name = "domain.agent.submitted_output", skip(self))]
    pub async fn submitted_output(
        &self,
        agent_id: AgentId,
    ) -> Result<Option<serde_json::Value>, AgentError> {
        Ok(self.sessions.submitted_output(agent_id).await?)
    }

    /// Workflow-spawned agents are filtered out; see
    /// [`Self::list_for_workflow_run`] for those.
    #[instrument(name = "domain.agent.list_for_project", skip(self, sub))]
    pub async fn list_for_project(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
    ) -> Result<Vec<Agent>, AgentError> {
        sub.can(AuthVerb::Read, AuthResource::Agent(project_id, None))?;
        Audit::record_action_if_unset("agent.list_for_project");
        Audit::record_project_id(project_id);
        let query = es_entity::PaginatedQueryArgs {
            first: 100,
            after: None,
        };
        let result = self
            .repo
            .list_for_project_id_by_created_at(
                project_id,
                query,
                es_entity::ListDirection::Descending,
            )
            .await?;
        // Covered by the `idx_agents_project_id_user_owned` partial index.
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
        project_id: ProjectId,
        run_id: WorkflowRunId,
    ) -> Result<Vec<Agent>, AgentError> {
        sub.can(AuthVerb::Read, AuthResource::Agent(project_id, None))?;
        Audit::record_action_if_unset("agent.list_for_workflow_run");
        Audit::record_project_id(project_id);
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

    #[instrument(name = "domain.agent.list_for_project_in_op", skip(self, op))]
    pub(crate) async fn list_for_project_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        project_id: ProjectId,
    ) -> Result<Vec<Agent>, AgentError> {
        Audit::record_project_id(project_id);
        const PAGE_SIZE: usize = 100;
        let mut all = Vec::new();
        let mut query = es_entity::PaginatedQueryArgs {
            first: PAGE_SIZE,
            after: None,
        };
        loop {
            let mut result = self
                .repo
                .list_for_project_id_by_created_at_in_op(
                    &mut *op,
                    project_id,
                    query,
                    es_entity::ListDirection::Descending,
                )
                .await?;
            all.append(&mut result.entities);
            match result.into_next_query() {
                Some(next) => query = next,
                None => break,
            }
        }
        Ok(all)
    }

    /// Lookup the workflow agent for `(run_id, name)`. Re-entrancy primitive
    /// for [`crate::workflow::Executor`]: on retry we reuse the existing
    /// agent (preserving its `sandbox_attached` claim and session history)
    /// instead of creating a fresh one that would collide on the write slot.
    #[instrument(name = "domain.agent.find_for_workflow_step_in_op", skip(self, op))]
    pub async fn find_for_workflow_step_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        run_id: WorkflowRunId,
        name: &str,
    ) -> Result<Option<Agent>, AgentError> {
        let query = es_entity::PaginatedQueryArgs {
            first: 100,
            after: None,
        };
        let result = self
            .repo
            .list_for_workflow_run_id_by_created_at_in_op(
                &mut *op,
                Some(run_id),
                query,
                es_entity::ListDirection::Ascending,
            )
            .await?;
        Ok(result.entities.into_iter().find(|a| a.name == name))
    }

    #[instrument(name = "domain.agent.delete", skip(self, sub))]
    pub async fn delete(
        &self,
        sub: &AuthSubject,
        id: impl Into<AgentId> + std::fmt::Debug,
    ) -> Result<(), AgentError> {
        let id = id.into();
        let agent = self.repo.find_by_id(id).await?;
        sub.can(
            AuthVerb::Delete,
            AuthResource::Agent(agent.project_id, Some(agent.id)),
        )?;
        Audit::record_action_if_unset("agent.delete");
        Audit::record_project_id(agent.project_id);
        Audit::record_agent_id(id);
        let mut op = self.repo.begin_op().await?;
        self.delete_in_op(&mut op, id).await?;
        op.commit().await?;
        self.invalidate_agent_cache(id);
        Ok(())
    }

    #[instrument(name = "domain.agent.delete_in_op", skip(self, op))]
    pub async fn delete_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        id: impl Into<AgentId> + std::fmt::Debug,
    ) -> Result<(), AgentError> {
        let id = id.into();
        let mut agent = self.repo.find_by_id_in_op(&mut *op, id).await?;
        Audit::record_project_id(agent.project_id);
        Audit::record_agent_id(id);
        // Mirror `detach_sandbox`: agent-side detach event + sandbox-side
        // detach. The session detach notification is folded into the
        // session delete below to avoid loading the session twice.
        let session_detach = if let Some(sandbox_id) = agent.attached_sandbox_id() {
            if agent.sandbox_detached(sandbox_id).did_execute() {
                self.repo.update_in_op(&mut *op, &mut agent).await?;
            }
            let sandbox = self
                .sandboxes
                .detach_from_agent_in_op(op, sandbox_id, id)
                .await?;
            Some((sandbox.name, session::message::SandboxOperation::Detach))
        } else {
            None
        };
        self.sessions
            .delete_for_agent_in_op(op, id, session_detach)
            .await?;
        self.repo.delete_in_op(op, agent).await?;
        Ok(())
    }

    #[instrument(
        name = "domain.agent.cascade_delete_for_workflow_id_in_op",
        skip(self, op)
    )]
    pub async fn cascade_delete_for_workflow_id_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        workflow_id: WorkflowDefinitionId,
    ) -> Result<(), AgentError> {
        const PAGE_SIZE: usize = 100;
        loop {
            let result = self
                .repo
                .list_for_workflow_id_by_created_at_in_op(
                    &mut *op,
                    Some(workflow_id),
                    es_entity::PaginatedQueryArgs {
                        first: PAGE_SIZE,
                        after: None,
                    },
                    es_entity::ListDirection::Ascending,
                )
                .await?;
            if result.entities.is_empty() {
                break;
            }
            for agent in result.entities {
                self.delete_in_op(op, agent.id).await?;
            }
        }
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
        let project_id = agent.project_id;
        subject.can(
            AuthVerb::Update,
            AuthResource::Agent(project_id, Some(agent.id)),
        )?;
        Audit::record_action_if_unset("agent.attach_sandbox");
        Audit::record_project_id(project_id);
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
            .attach_to_agent_in_op(&mut op, project_id, sandbox_id, agent_id, mode)
            .await?;

        let (kind, scope) = sandbox.kind_and_scope();
        let push_policy = self.sandboxes.push_policy_text(&sandbox.mode);
        self.sessions
            .sandbox_notification_in_op(
                &mut op,
                agent_id,
                sandbox.name,
                session::message::SandboxOperation::Attach {
                    agent_mode: format!("{mode:?}").to_lowercase(),
                    kind: kind.to_string(),
                    cwd: sandbox.cwd,
                    scope,
                    push_policy,
                },
            )
            .await?;

        self.refresh_skills_block_in_op(&mut op, agent_id, project_id, Some(sandbox_id))
            .await;

        op.commit().await?;

        self.invalidate_agent_cache(agent_id);

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
            AuthResource::Agent(agent.project_id, Some(agent.id)),
        )?;
        Audit::record_action_if_unset("agent.detach_sandbox");
        Audit::record_project_id(agent.project_id);
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

        self.refresh_skills_block_in_op(&mut op, agent_id, agent.project_id, None)
            .await;

        op.commit().await?;

        self.invalidate_agent_cache(agent_id);

        Ok(agent)
    }

    /// Pre-step helper for the workflow runtime. Detaches a single
    /// conflicting Writer when the caller is about to attach in `Write`
    /// mode; no-op otherwise.
    ///
    /// `caller_workflow_id` is `Some` for workflow-runtime callers and
    /// gates the cross-workflow protection: a workflow Writer is only
    /// stolen when it belongs to the same workflow definition (a stale
    /// claim from this workflow's prior run, or a sibling step of the
    /// same run). Different workflows are never stolen from.
    ///
    /// Conflict policy:
    /// - `workflow_mode == Read`: never detach (Read is unbounded; we
    ///   coexist with whatever's there).
    /// - `workflow_mode == Write` and a non-workflow agent currently
    ///   holds Write: run the full detach choreography (agent event,
    ///   sandbox detach, session notification, skills refresh).
    /// - `workflow_mode == Write` and the current Writer is a workflow
    ///   agent of the *same* workflow as `caller_workflow_id`: detach
    ///   it (same-workflow steal — handles a prior run's stale claim).
    /// - `workflow_mode == Write` and the current Writer is a workflow
    ///   agent of a *different* workflow (or `caller_workflow_id` is
    ///   `None`): no-op; protects concurrent unrelated runs.
    /// - `workflow_mode == Write` and only Readers (or nothing) are
    ///   attached: no-op; Read+Write is allowed at the entity layer.
    ///
    /// Returns the detached agent id (single-element vec or empty) so
    /// the caller can drop its `cached_dynamic_blocks` entry after
    /// committing.
    #[instrument(
        name = "domain.agent.detach_conflicting_writer_in_op",
        skip(self, op),
        fields(%sandbox_id, ?workflow_mode, ?caller_workflow_id)
    )]
    pub async fn detach_conflicting_writer_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        sandbox_id: SandboxId,
        workflow_mode: SandboxAgentMode,
        caller_workflow_id: Option<WorkflowDefinitionId>,
    ) -> Result<Vec<AgentId>, AgentError> {
        if workflow_mode != SandboxAgentMode::Write {
            return Ok(Vec::new());
        }
        Audit::record_action_if_unset("agent.detach_conflicting_writer");
        Audit::record_sandbox_id(sandbox_id);

        let sandbox = self.sandboxes.find_by_id_in_op(op, sandbox_id).await?;
        let writer_id = sandbox
            .attached_agents
            .iter()
            .find(|(_, m)| *m == SandboxAgentMode::Write)
            .map(|(id, _)| *id);
        let Some(agent_id) = writer_id else {
            return Ok(Vec::new());
        };

        let mut agent = self.repo.find_by_id_in_op(&mut *op, agent_id).await?;
        if let Some(writer_workflow_id) = agent.workflow_id {
            match caller_workflow_id {
                Some(caller_id) if caller_id == writer_workflow_id => {
                    // Same workflow: stale claim from a prior run (or a
                    // sibling step of this run) — fall through and detach.
                }
                _ => return Ok(Vec::new()),
            }
        }
        let project_id = agent.project_id;
        if agent.sandbox_detached(sandbox_id).did_execute() {
            self.repo.update_in_op(&mut *op, &mut agent).await?;
        }
        let sandbox = self
            .sandboxes
            .detach_from_agent_in_op(op, sandbox_id, agent_id)
            .await?;
        self.sessions
            .sandbox_notification_in_op(
                op,
                agent_id,
                sandbox.name,
                session::message::SandboxOperation::Detach,
            )
            .await?;
        self.refresh_skills_block_in_op(op, agent_id, project_id, None)
            .await;
        Ok(vec![agent_id])
    }

    /// Recomputes the `Skills` system block for `agent_id` scoped to
    /// `sandbox_id` and pushes it. Idempotent: when the block content matches
    /// the latest persisted skills block no event is emitted. Errors from the
    /// skills service or session push are logged and swallowed — sandbox
    /// attach/detach must not fail because of a transient skills-context
    /// problem.
    ///
    /// Resolves the FULL agent scope (project + global + mounted spaces +
    /// sandbox) so the refreshed block matches what `cached_dynamic_blocks`
    /// would build on the next `send_message`. Calling
    /// `skills_context_for_agent` here would silently drop space-tier
    /// skills (it carries an empty `mounted_space_ids`).
    async fn refresh_skills_block_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        agent_id: AgentId,
        project_id: ProjectId,
        sandbox_id: Option<SandboxId>,
    ) {
        let mounted_space_ids = match self.space_mounts.space_ids_for_project(project_id).await {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(error = %e, "space_ids_for_project failed during skills refresh");
                Vec::new()
            }
        };
        let scope = scope::AgentScope {
            project_id,
            attached_sandbox_id: sandbox_id,
            mounted_space_ids,
        };
        let skills_text = match self.skills.skills_context_for_scope(&scope).await {
            Ok(Some(text)) => text,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(error = %e, "skills_context_for_scope failed during refresh");
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

    /// Drops `agent_id`'s entry from the dynamic-blocks cache so the next
    /// `cached_dynamic_blocks` call rebuilds. Used when `agent_id`'s scope
    /// changes (e.g. sandbox attach/detach) without bumping the global
    /// `ContextGeneration`.
    pub(crate) fn invalidate_agent_cache(&self, agent_id: AgentId) {
        if let Ok(mut cache) = self.context_cache.write() {
            cache.remove(&agent_id);
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
            AuthResource::Agent(agent.project_id, Some(agent.id)),
        )?;
        Audit::record_action_if_unset("agent.chat_history");
        Audit::record_project_id(agent.project_id);
        Audit::record_agent_id(agent_id);
        Ok(self.sessions.chat_history(agent_id, last_n).await?)
    }

    #[instrument(name = "domain.agent.find_session", skip(self, sub))]
    pub async fn find_session(
        &self,
        sub: &AuthSubject,
        agent_id: AgentId,
    ) -> Result<session::AgentSession, AgentError> {
        let agent = self.repo.find_by_id(agent_id).await?;
        sub.can(
            AuthVerb::Read,
            AuthResource::Agent(agent.project_id, Some(agent.id)),
        )?;
        Audit::record_action_if_unset("agent.find_session");
        Audit::record_project_id(agent.project_id);
        Audit::record_agent_id(agent_id);
        Ok(self.sessions.find_for_agent(agent_id).await?)
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
            AuthResource::Agent(agent.project_id, Some(agent.id)),
        )?;
        Audit::record_action_if_unset("agent.thread_infos");
        Audit::record_project_id(agent.project_id);
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
            AuthResource::Agent(agent.project_id, Some(agent.id)),
        )?;
        Audit::record_action_if_unset("agent.thread_messages");
        Audit::record_project_id(agent.project_id);
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
            AuthResource::Agent(agent.project_id, Some(agent.id)),
        )?;
        Audit::record_action_if_unset("agent.thread_system_view");
        Audit::record_project_id(agent.project_id);
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
            AuthResource::Agent(agent.project_id, Some(agent.id)),
        )?;
        Audit::record_action_if_unset("agent.export_thread");
        Audit::record_project_id(agent.project_id);
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
        self.send_message_with_choice(subject, id, prompt, None)
            .await
    }

    /// `tool_choice` is applied to the initial prompt only. The agent
    /// loop's per-turn re-prompts inherit `Auto` from the session, so a
    /// forced choice is a one-shot nudge rather than a sticky setting.
    #[instrument(name = "domain.agent.send_message_with_choice", skip(self, prompt))]
    pub async fn send_message_with_choice(
        &self,
        subject: AuthSubject,
        id: AgentId,
        prompt: String,
        tool_choice: Option<llm::prompt::ToolChoice>,
    ) -> Result<tokio::sync::mpsc::Receiver<ChatOutputEvent>, AgentError> {
        let agent = self.repo.find_by_id(id).await?;

        // Agents may only message peers in their own project; anonymous rejected.
        match &subject {
            AuthSubject::User(_) | AuthSubject::ExportedAgent(_, _, _) => {}
            AuthSubject::Agent(project, _, _)
            | AuthSubject::AgentOnBehalfOfUser(_, project, _, _)
                if *project == agent.project_id => {}
            _ => return Err(AgentError::Unauthorized),
        }

        Audit::record_action_if_unset("agent.send_message");
        Audit::record_project_id(agent.project_id);
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
        let dynamic_blocks = self.cached_dynamic_blocks(&agent).await;
        let mut proposed_system_blocks = system_prompt::system_blocks_for_role(
            agent.agent_role,
            &self.toolsets,
            &agent_subject,
            &agent.project_name,
            agent.output_schema.as_ref(),
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
                        "Message queued — session is waiting for tool results that will never arrive. Consider creating a new project."
                    }
                    session::AgentSessionResponse::AwaitingAssistantResponse => {
                        "Message queued — session is waiting for an assistant response. Try again shortly."
                    }
                    _ => "Message queued — session is busy.",
                };
                let tx_clone = tx.clone();
                let message = msg.to_string();
                tokio::spawn(async move {
                    emit_event(&tx_clone, ChatOutputEvent::Service { message });
                });
                return Ok(rx);
            }
        }

        emit_event(
            &tx,
            ChatOutputEvent::UserMessage {
                source,
                text: prompt,
            },
        );

        let mut prompt_state = self
            .sessions
            .next_prompt(id, session::TargetThread::Main)
            .await?;
        if let Some(choice) = tool_choice {
            prompt_state.tool_choice = Some(choice);
        }

        self.drive_session_loop(id, agent_subject, tx, prompt_state)
            .await?;

        Ok(rx)
    }

    /// Re-drive an in-flight LLM call for a workflow agent on retry. Uses
    /// [`Sessions::pending_prompt`] which, when the last `PromptSent` has no
    /// matching `AssistantResponseReceived`, rebuilds the original prompt
    /// from the persisted definition. Returns `Ok(None)` when the prior turn
    /// closed cleanly (success or recorded error) — caller should fall back
    /// to [`Self::send_message`] to start a fresh turn.
    #[instrument(name = "domain.agent.resume_message", skip(self))]
    pub async fn resume_message(
        &self,
        subject: AuthSubject,
        id: AgentId,
    ) -> Result<Option<tokio::sync::mpsc::Receiver<ChatOutputEvent>>, AgentError> {
        let agent = self.repo.find_by_id(id).await?;

        match &subject {
            AuthSubject::User(_) | AuthSubject::ExportedAgent(_, _, _) => {}
            AuthSubject::Agent(project, _, _)
            | AuthSubject::AgentOnBehalfOfUser(_, project, _, _)
                if *project == agent.project_id => {}
            _ => return Err(AgentError::Unauthorized),
        }

        Audit::record_action_if_unset("agent.resume_message");
        Audit::record_project_id(agent.project_id);
        Audit::record_agent_id(id);
        if let Some(wf) = agent.workflow_id {
            Audit::record_workflow_id(wf);
        }
        if let Some(run) = agent.workflow_run_id {
            Audit::record_workflow_run_id(run);
        }

        let Some(prompt_state) = self.sessions.pending_prompt(id).await? else {
            return Ok(None);
        };

        let agent_subject = match subject.originating_user_id() {
            Some(user_id) => agent.auth_subject_for_user(user_id),
            None => agent.auth_subject(),
        };

        let (tx, rx) = tokio::sync::mpsc::channel::<ChatOutputEvent>(64);
        self.drive_session_loop(id, agent_subject, tx, prompt_state)
            .await?;
        Ok(Some(rx))
    }

    /// Send the initial `prompt_state` to the LLM and spawn the response
    /// loop. Shared by [`Self::send_message`] (fresh turn) and
    /// [`Self::resume_message`] (retried turn from a persisted `PromptSent`).
    /// The chain travels inside `prompt_state` itself — the session
    /// is the only source of truth for which chain to dispatch with.
    async fn drive_session_loop(
        &self,
        id: AgentId,
        agent_subject: AuthSubject,
        tx: tokio::sync::mpsc::Sender<ChatOutputEvent>,
        prompt_state: llm::Prompt,
    ) -> Result<(), AgentError> {
        let model_name = prompt_state.chain.primary.name.clone();
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
                        emit_event(&tx, ChatOutputEvent::Error { message: msg });
                        return;
                    }
                    Err(_) => {
                        let msg = "prompt response channel closed".to_string();
                        let _ = sessions
                            .assistant_response_failed(id, current_model.clone(), msg.clone())
                            .await;
                        emit_event(&tx, ChatOutputEvent::Error { message: msg });
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
                        emit_event(
                            &tx,
                            ChatOutputEvent::Error {
                                message: e.to_string(),
                            },
                        );
                        return;
                    }
                };

                if !streamed {
                    forward_response(response, &tx);
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

                        // The session detects a terminal `submit_output`
                        // call inside `add_tool_results` and returns
                        // `Done` — the loop breaks below without a
                        // follow-up prompt.
                        let post = match sessions.add_tool_results(id, results).await {
                            Ok(r) => r,
                            Err(e) => {
                                emit_event(
                                    &tx,
                                    ChatOutputEvent::Error {
                                        message: e.to_string(),
                                    },
                                );
                                return;
                            }
                        };

                        if matches!(post, session::AgentSessionResponse::Done) {
                            break;
                        }

                        match sessions.next_prompt(id, session::TargetThread::Main).await {
                            Ok(p) => p,
                            Err(e) => {
                                emit_event(
                                    &tx,
                                    ChatOutputEvent::Error {
                                        message: e.to_string(),
                                    },
                                );
                                return;
                            }
                        }
                    }
                    session::AgentSessionResponse::PromptPending { target } => {
                        match sessions.next_prompt(id, target).await {
                            Ok(p) => p,
                            Err(e) => {
                                emit_event(
                                    &tx,
                                    ChatOutputEvent::Error {
                                        message: e.to_string(),
                                    },
                                );
                                return;
                            }
                        }
                    }
                    _ => break,
                };

                current_model = next_prompt.chain.primary.name.clone();
                let (request, rx_next) = llm::PromptRequest::new(next_prompt);
                if prompt_requests.send(request).await.is_err() {
                    emit_event(
                        &tx,
                        ChatOutputEvent::Error {
                            message: "prompt request channel closed".to_string(),
                        },
                    );
                    return;
                }
                next = rx_next.await;
            }

            emit_event(
                &tx,
                ChatOutputEvent::AssistantDone {
                    turns: turn,
                    input_tokens,
                    output_tokens,
                    duration_ms: None,
                    cost_usd: None,
                },
            );
        });
        Ok(())
    }
}

fn emit_event(tx: &tokio::sync::mpsc::Sender<ChatOutputEvent>, event: ChatOutputEvent) {
    match tx.try_send(event) {
        Ok(()) | Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            tracing::debug!("dropping chat output event because receiver is backpressured");
        }
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
                    emit_event(tx, chat_event);
                }
                acc.process(&delta);
            }
            Err(e) => {
                let msg = e.to_string();
                emit_event(
                    tx,
                    ChatOutputEvent::Error {
                        message: msg.clone(),
                    },
                );
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
    let n = calls.len();
    let dispatches = calls.into_iter().map(|tu| {
        let toolsets = toolsets.clone();
        let subject = subject.clone();
        async move {
            let name = tu.name.clone();
            let id = tu.id.clone();
            let args = tu.input.as_object().cloned();
            let res = toolsets.call_top_level_tool(&subject, &name, args).await;
            let result = match res {
                Ok(r) => llm::ToolUseResult {
                    tool_use_id: id,
                    content: call_result_to_text(&r),
                    is_error: r.is_error.unwrap_or(false),
                },
                Err(e) => llm::ToolUseResult {
                    tool_use_id: id,
                    content: flatten_error(&e),
                    is_error: true,
                },
            };
            (name, result)
        }
    });

    let outcomes = futures::future::join_all(dispatches).await;

    let mut results = Vec::with_capacity(n);
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
        emit_event(
            tx,
            ChatOutputEvent::ToolResult {
                name,
                is_error: result.is_error,
                content,
            },
        );
        results.push(result);
    }
    results
}

/// Render an error chain to a single concise line for the model.
/// Walks `.source()` to the deepest cause (so wrapper layers like
/// `ToolSetsError -> ProjectError -> SpaceError` collapse to the leaf),
/// then strips the conventional `"TypeName - "` thiserror prefix.
fn flatten_error(err: &dyn std::error::Error) -> String {
    let mut current: &dyn std::error::Error = err;
    while let Some(src) = current.source() {
        current = src;
    }
    let leaf = current.to_string();
    leaf.split_once(" - ")
        .map(|(_, rest)| rest.to_string())
        .unwrap_or(leaf)
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

fn forward_response(
    response: llm::PromptResponse,
    tx: &tokio::sync::mpsc::Sender<ChatOutputEvent>,
) {
    for block in response.content {
        match block {
            llm::prompt::AssistantBlock::Text { text, .. } => {
                emit_event(tx, ChatOutputEvent::AssistantText { text });
            }
            llm::prompt::AssistantBlock::ToolUse { name, input, .. } => {
                emit_event(tx, ChatOutputEvent::ToolCallStart { name });
                emit_event(
                    tx,
                    ChatOutputEvent::ToolCallInputDelta {
                        partial_json: input.to_string(),
                    },
                );
            }
            llm::prompt::AssistantBlock::Thinking { text, .. } => {
                emit_event(tx, ChatOutputEvent::Thinking { text });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, LazyLock,
    };
    use std::time::Duration;

    use rmcp::model::{CallToolResult, Content, JsonObject};

    use super::*;
    use crate::toolset::{ToolSets, ToolSetsError, TopLevelTool};

    struct StubTool {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl TopLevelTool for StubTool {
        fn name(&self) -> &str {
            "stub"
        }

        fn description(&self) -> &str {
            "stub tool"
        }

        fn input_schema(&self) -> &serde_json::Value {
            static SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
                serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                })
            });
            &SCHEMA
        }

        async fn call(
            &self,
            _subject: &AuthSubject,
            _arguments: Option<JsonObject>,
        ) -> Result<CallToolResult, ToolSetsError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(CallToolResult::success(vec![Content::text("done")]))
        }
    }

    #[tokio::test]
    async fn fan_out_tool_calls_does_not_wait_for_backpressured_chat_receiver() {
        let toolsets = Arc::new(ToolSets::empty_for_test());
        let calls = Arc::new(AtomicUsize::new(0));
        toolsets.register_top_level(StubTool {
            calls: Arc::clone(&calls),
        });

        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        tx.try_send(ChatOutputEvent::Service {
            message: "fill channel".to_string(),
        })
        .expect("channel should accept initial event");

        let tool_calls = vec![llm::RequestToolUse {
            id: "toolu_1".to_string(),
            name: "stub".to_string(),
            input: serde_json::json!({}),
        }];

        let results = tokio::time::timeout(
            Duration::from_millis(100),
            fan_out_tool_calls(&toolsets, &AuthSubject::Anonymous, tool_calls, &tx),
        )
        .await
        .expect("tool result persistence must not wait on chat stream backpressure");

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].content, "done");
        assert!(!results[0].is_error);
    }
}
