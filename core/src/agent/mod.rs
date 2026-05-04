pub mod config;
mod entity;
pub mod error;
mod pi_export;
pub mod repo;
pub mod scope;
pub mod session;
mod system_prompt;

pub use scope::AgentScope;

use std::sync::{Arc, OnceLock};

use crate::audit::Audit;
use crate::note::Notes;
use crate::skill::Skills;
use crate::toolset::ToolSets;

/// Lead → `ProjectAdmin`; Agent → `ProjectMember` (read-only). Sandbox
/// scopes are added later via [`Agent::sandbox_attached`].
fn default_authz_scopes(role: AgentRole, project_id: ProjectId) -> Vec<AuthScope> {
    match role {
        AgentRole::ProjectLead => vec![AuthScope::ProjectAdmin(project_id)],
        AgentRole::Agent => vec![AuthScope::ProjectMember(project_id)],
    }
}

use tracing::instrument;

use crate::primitives::{
    AgentId, AuthResource, AuthScope, AuthSubject, AuthVerb, ChatOutputEvent, ContextGeneration,
    ProjectId, SandboxId, WorkflowDefinitionId, WorkflowRunId,
};
use crate::sandbox::{SandboxAgentMode, Sandboxes};
pub use config::{AgentsConfig, ModelDefaults, RoleConfig};
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
    /// Late-bound: `Projects::new` takes `Arc<Agents>`, so we set this
    /// after both are constructed via `Agents::set_projects`. Used only
    /// to render the dynamic `<spaces>` system block.
    projects: Arc<OnceLock<Arc<crate::project::Projects>>>,
    config: AgentsConfig,
    toolsets: Arc<ToolSets>,
    prompt_requests: llm::PromptRequestChannel,
    context_generation: ContextGeneration,
    context_cache:
        Arc<std::sync::RwLock<std::collections::HashMap<AgentId, CachedAgentContext>>>,
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
    ) -> Self {
        Self {
            repo: AgentRepo::new(pool),
            sessions: Sessions::new(pool),
            sandboxes,
            skills,
            notes,
            projects: Arc::new(OnceLock::new()),
            config,
            toolsets,
            prompt_requests,
            context_generation,
            context_cache: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
        }
    }

    /// Late-binds the `Projects` service so the dynamic `<spaces>`
    /// block can be rendered. `Projects::new` takes `Arc<Agents>`, so
    /// the cyclic dependency forces this two-step wiring at app init.
    /// Idempotent — second call is a no-op.
    pub fn set_projects(&self, projects: Arc<crate::project::Projects>) {
        let _ = self.projects.set(projects);
    }

    /// Hot-path lookup of dynamic system blocks for a specific agent. Reads
    /// `ContextGeneration` atomically; only hits DB when the generation has
    /// bumped. Per-agent cache so attached sandbox (and future per-agent
    /// scope dimensions) doesn't bleed across agents in the same project.
    async fn cached_dynamic_blocks(
        &self,
        agent: &Agent,
    ) -> Vec<session::message::SystemBlock> {
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

        let notes_block = match &self.notes {
            Some(notes) => notes
                .pinned_context_for_project(project_id)
                .await
                .ok()
                .flatten()
                .map(|text| session::message::SystemBlock::Notes { text }),
            None => None,
        };
        let skills_block = match self.projects.get() {
            Some(projects) => match scope::AgentScope::for_agent(agent, projects).await {
                Ok(scope) => self
                    .skills
                    .skills_context_for_scope(&scope)
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
            },
            // Pre-Projects-wired (test) path: same single-tier fallback.
            None => self
                .skills
                .skills_context_for_agent(project_id, agent.attached_sandbox_id())
                .await
                .ok()
                .flatten()
                .map(|text| session::message::SystemBlock::Skills { text }),
        };
        let spaces_block = match self.projects.get() {
            Some(projects) => projects
                .spaces_context_for_project(project_id)
                .await
                .ok()
                .flatten()
                .map(|text| session::message::SystemBlock::Spaces { text }),
            None => None,
        };

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
            )
            .await?;
        op.commit().await?;
        Ok(agent)
    }

    /// Caller commits the op. Stamping `(workflow_id, workflow_run_id)`
    /// is what excludes the agent from [`Self::list_for_project`].
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
            AgentRole::Agent,
            name,
            attach_sandbox,
            &project_name,
            Some(workflow_id),
            Some(workflow_run_id),
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
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    #[instrument(name = "domain.agent.create_in_op", skip(self, op))]
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

        let authz_scopes = default_authz_scopes(agent_role, project_id);

        let mut new_agent_builder = NewAgent::builder();
        new_agent_builder
            .id(id)
            .project_id(project_id)
            .agent_role(agent_role)
            .name(name)
            .authz_scopes(authz_scopes)
            .project_name(project_name);
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
            &agent.project_name,
        );

        if let Some(notes) = &self.notes {
            if let Ok(Some(pinned_content)) = notes.pinned_context_for_project(project_id).await {
                system_blocks.push(session::message::SystemBlock::Notes {
                    text: pinned_content,
                });
            }
        }

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

        let initial_skills = match self.projects.get() {
            Some(projects) => {
                match scope::AgentScope::for_agent(&agent, projects).await {
                    Ok(mut scope) => {
                        // Reflect the (just-applied) attachment in the
                        // initial scope — `attached_sandbox_id()` reads
                        // the entity, which we updated above.
                        scope.attached_sandbox_id = initial_sandbox_id;
                        self.skills.skills_context_for_scope(&scope).await
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "AgentScope::for_agent failed at create; rendering initial skills block without space tier");
                        self.skills
                            .skills_context_for_agent(project_id, initial_sandbox_id)
                            .await
                    }
                }
            }
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

        if let Some(projects) = self.projects.get() {
            if let Ok(Some(spaces_content)) = projects.spaces_context_for_project(project_id).await
            {
                system_blocks.push(session::message::SystemBlock::Spaces {
                    text: spaces_content,
                });
            }
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
                .attach_to_agent_in_op(op, project_id, sandbox_id, agent.id, mode)
                .await?;

            let (kind, scope) = sandbox.kind_and_scope();
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
        let query = es_entity::PaginatedQueryArgs {
            first: 100,
            after: None,
        };
        let result = self
            .repo
            .list_for_project_id_by_created_at_in_op(
                &mut *op,
                project_id,
                query,
                es_entity::ListDirection::Descending,
            )
            .await?;
        Ok(result.entities)
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

    /// Recomputes the `Skills` system block for `agent_id` scoped to
    /// `sandbox_id` and pushes it. Idempotent: when the block content matches
    /// the latest persisted skills block no event is emitted. Errors from the
    /// skills service or session push are logged and swallowed — sandbox
    /// attach/detach must not fail because of a transient skills-context
    /// problem.
    async fn refresh_skills_block_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        agent_id: AgentId,
        project_id: ProjectId,
        sandbox_id: Option<SandboxId>,
    ) {
        let skills_text = match self
            .skills
            .skills_context_for_agent(project_id, sandbox_id)
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

    /// Drops `agent_id`'s entry from the dynamic-blocks cache so the next
    /// `cached_dynamic_blocks` call rebuilds. Used when `agent_id`'s scope
    /// changes (e.g. sandbox attach/detach) without bumping the global
    /// `ContextGeneration`.
    fn invalidate_agent_cache(&self, agent_id: AgentId) {
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
