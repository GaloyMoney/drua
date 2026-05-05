pub(crate) mod cron_job;
pub mod definition;
pub mod entity;
pub mod error;
pub mod executor;
pub mod importer;
pub(crate) mod job;
pub(crate) mod repo;
pub mod run;
pub mod yaml;

pub use importer::WorkflowsImporter;

use std::sync::Arc;

use rand::RngCore;
use tracing::instrument;

use crate::agent::Agents;
use crate::primitives::*;
use crate::sandbox::Sandboxes;
use crate::skill::Skills;
use crate::user::Users;

pub const WORKFLOW_DOC_TYPE: drua_library::DocType = drua_library::DocType::new("workflow");

pub use definition::{
    next_cron_fire_at, parse_cron_schedule, parse_timezone, WorkflowSandboxDecl, WorkflowStepDef,
    WorkflowTrigger,
};
pub use entity::*;
pub use error::*;
pub use run::{StepResult, WorkflowRun, WorkflowRunRepo, WorkflowRunState};

use crate::agent::session::{AgentSessionId, ToolCallSummary};

/// Filter passed to [`Workflows::list_runs_filtered`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStateFilter {
    Succeeded,
    Failed,
    Running,
}

impl RunStateFilter {
    pub fn matches(&self, state: WorkflowRunState) -> bool {
        matches!(
            (self, state),
            (Self::Succeeded, WorkflowRunState::Succeeded)
                | (Self::Failed, WorkflowRunState::Failed)
                | (
                    Self::Running,
                    WorkflowRunState::Pending | WorkflowRunState::Running
                )
        )
    }
}

/// Output of [`Workflows::find_run_with_step_details`].
pub struct RunWithStepDetails {
    pub run: WorkflowRun,
    pub steps: Vec<StepWithDetails>,
}

pub struct StepWithDetails {
    pub step_name: String,
    pub error: Option<String>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub output: Option<serde_json::Value>,
    pub details: Option<StepAgentDetails>,
}

pub struct StepAgentDetails {
    pub agent_id: AgentId,
    pub session_id: AgentSessionId,
    pub summary: ToolCallSummary,
}

/// Mirrors the executor's agent-name format:
/// `workflow-{run_id_short}-{step_name}`. Step names may contain
/// hyphens themselves, so consumers must match by prefix + suffix.
pub(crate) fn workflow_agent_name_prefix(run_id: WorkflowRunId) -> String {
    let s = run_id.to_string();
    let short = s.split_once('-').map(|(p, _)| p.to_string()).unwrap_or(s);
    format!("workflow-{short}-")
}

fn matches_step_agent(agent_name: &str, prefix: &str, step_name: &str) -> bool {
    agent_name
        .strip_prefix(prefix)
        .is_some_and(|rest| rest == step_name)
}

use cron_job::{cron_queue_id, TriggerCronConfig, TriggerCronJobInitializer};
use job::{ExecuteRunConfig, ExecuteRunJobInitializer};
use repo::WorkflowDefinitionRepo;
use run::entity::NewWorkflowRun;

#[derive(Clone)]
pub struct Workflows {
    repo: WorkflowDefinitionRepo,
    run_repo: WorkflowRunRepo,
    skills: Arc<Skills>,
    users: Arc<Users>,
    /// Held so run-inspection commands can join a run's per-step
    /// agents and their sessions for tool-call telemetry. Not used by
    /// any mutation path.
    agents: Arc<Agents>,
    execute_run_spawner: ::job::JobSpawner<ExecuteRunConfig>,
    cron_spawner: ::job::JobSpawner<TriggerCronConfig>,
    /// Cloned `Jobs` handle so `await_run_completion` can block on the
    /// `ExecuteRun` job (whose id == run id) without polling.
    jobs: ::job::Jobs,
}

impl Workflows {
    /// Wires the workflow service: registers the `ExecuteRun` and
    /// `TriggerCron` job initializers with `jobs`. Cron schedules
    /// persist as job rows, so no startup-recovery is needed — the
    /// poller will pick them up on its first tick.
    #[allow(clippy::too_many_arguments)]
    pub fn init(
        pool: &sqlx::PgPool,
        library: drua_library::Library,
        skills: Arc<Skills>,
        agents: Arc<Agents>,
        sandboxes: Arc<Sandboxes>,
        users: Arc<Users>,
        jobs: &mut ::job::Jobs,
    ) -> Self {
        let execute_run_spawner = jobs.add_initializer(ExecuteRunJobInitializer::new(
            WorkflowRunRepo::new(pool),
            WorkflowDefinitionRepo::new_without_library(pool),
            Arc::clone(&agents),
            Arc::clone(&skills),
            sandboxes,
        ));
        let cron_spawner = jobs.add_initializer(TriggerCronJobInitializer::new(
            WorkflowDefinitionRepo::new_without_library(pool),
            WorkflowRunRepo::new(pool),
            execute_run_spawner.clone(),
        ));
        Self {
            repo: WorkflowDefinitionRepo::new(pool, library),
            run_repo: WorkflowRunRepo::new(pool),
            skills,
            users,
            agents,
            execute_run_spawner,
            cron_spawner,
            jobs: jobs.clone(),
        }
    }

    /// Reverse-sync entry point: persist a `ParsedWorkflow` produced
    /// by the library importer. Creates or updates depending on
    /// whether the workflow already exists. `Ok(None)` signals
    /// idempotency — the existing entity's `file_hash` matches the
    /// incoming bytes, so the caller should skip search re-upsert +
    /// embed re-spawn.
    pub(crate) async fn import_from_library<OP: es_entity::AtomicOperation>(
        &self,
        op: &mut OP,
        parsed: yaml::ParsedWorkflow,
        project_id: ProjectId,
    ) -> Result<Option<WorkflowDefinition>, WorkflowError> {
        let yaml::ParsedWorkflow {
            workflow_id,
            project_name,
            name,
            description,
            trigger,
            steps,
            sandboxes,
            original_path,
            rendered,
            ..
        } = parsed;

        Self::validate_trigger(&trigger)?;

        let file_hash = drua_library::GitFileHash::new(rendered);

        if let Some(mut existing) = self.repo.maybe_find_by_id(workflow_id).await? {
            // Same non-cron → cron rule as `update`: only a fresh
            // transition needs a spawn; cron → cron is picked up by
            // the in-flight job, cron → other is handled by the
            // runner's terminate-on-not-cron branch.
            let was_cron = matches!(existing.trigger, WorkflowTrigger::Cron { .. });
            if !existing
                .update_from_library(
                    Some(name.clone()),
                    Some(description.clone()),
                    Some(trigger),
                    Some(steps),
                    Some(sandboxes),
                    file_hash,
                )
                .did_execute()
            {
                return Ok(None);
            }
            self.repo.update_in_op(op, &mut existing).await?;
            let is_cron = matches!(existing.trigger, WorkflowTrigger::Cron { .. });
            if !was_cron && is_cron {
                self.register_cron_in_op(op, &existing).await?;
            }
            return Ok(Some(existing));
        }

        let trigger = match trigger {
            WorkflowTrigger::Webhook { provider, secret } if secret.is_empty() => {
                WorkflowTrigger::Webhook {
                    provider,
                    secret: Self::generate_webhook_secret(),
                }
            }
            other => other,
        };

        let mut builder = NewWorkflowDefinition::builder()
            .id(workflow_id)
            .project_id(project_id)
            .name(name)
            .trigger(trigger)
            .steps(steps)
            .sandboxes(sandboxes);
        if let Some(project) = project_name {
            builder = builder.project_name(project);
        }
        if let Some(desc) = description {
            builder = builder.description(desc);
        }
        builder = builder.original_path(original_path);
        let new = builder
            .build()
            .map_err(|e| WorkflowError::BuildEntity(e.to_string()))?;
        let created = self.repo.create_in_op(op, new).await?;
        self.register_cron_in_op(op, &created).await?;
        Ok(Some(created))
    }

    fn generate_webhook_secret() -> String {
        let mut bytes = [0u8; 16];
        rand::rng().fill_bytes(&mut bytes);
        format!("whsec_{}", hex::encode(bytes))
    }

    /// Reject cron triggers whose schedule or timezone won't parse so
    /// the failure surfaces at create-time, not on the first fire.
    fn validate_trigger(trigger: &WorkflowTrigger) -> Result<(), WorkflowError> {
        if let WorkflowTrigger::Cron { schedule, timezone } = trigger {
            parse_cron_schedule(schedule).map_err(WorkflowError::InvalidCronExpression)?;
            parse_timezone(timezone.as_deref()).map_err(WorkflowError::InvalidTimezone)?;
        }
        Ok(())
    }

    /// Resolve `skill:` references and validate sandbox references
    /// against the workflow's own declarations. Project skills win
    /// over sandbox-exported skills; an agent step backed by a
    /// `Preexisting` sandbox may also resolve its skill from that
    /// sandbox's exported set.
    async fn validate_steps(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
        steps: &[WorkflowStepDef],
        sandboxes: &[WorkflowSandboxDecl],
    ) -> Result<(), WorkflowError> {
        let mut decl_names: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for decl in sandboxes {
            if !decl_names.insert(decl.name()) {
                return Err(WorkflowError::DuplicateSandboxName(decl.name().to_string()));
            }
        }

        // Resolve every Preexisting decl up-front: look up the named
        // sandbox in the project + auth-check Read. The map is
        // consulted per-step so a sandbox-only skill counts as
        // resolved when its step targets a Preexisting sandbox.
        let preexisting_ids = self
            .resolve_preexisting_sandboxes(sub, project_id, sandboxes)
            .await?;

        // Collect every bad reference in a single pass so the operator
        // sees the full list (rather than fixing one and re-running).
        let mut missing_skills: Vec<(String, String)> = Vec::new();
        let mut undeclared_sandboxes: Vec<(String, String)> = Vec::new();

        for step in steps {
            match step {
                WorkflowStepDef::AgentStep {
                    name,
                    skill,
                    sandbox,
                    ..
                } => {
                    let preexisting_sandbox_id = sandbox
                        .as_deref()
                        .and_then(|n| preexisting_ids.get(n).copied());
                    // `find_by_name` resolves the project's mounted-space
                    // skills internally via the held `SpaceMounts`, so a
                    // workflow step referring to a space-scoped skill by
                    // name is reachable as long as the workflow's project
                    // mounts the space.
                    let found = self
                        .skills
                        .find_by_name(skill, Some(project_id), preexisting_sandbox_id)
                        .await
                        .map_err(|e| WorkflowError::Skill(e.to_string()))?;
                    if found.is_none() {
                        missing_skills.push((name.clone(), skill.clone()));
                    }

                    if let Some(sandbox_name) = sandbox {
                        if !decl_names.contains(sandbox_name.as_str()) {
                            undeclared_sandboxes.push((name.clone(), sandbox_name.clone()));
                        }
                    }
                }
            }
        }

        if !missing_skills.is_empty() {
            let listed = missing_skills
                .iter()
                .map(|(step, skill)| format!("step '{step}' → skill '{skill}'"))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(WorkflowError::SkillNotFound(format!(
                "{listed}. The `skill` field expects the NAME of a skill in this project; create skills first via the `skill` tool"
            )));
        }
        if !undeclared_sandboxes.is_empty() {
            let listed = undeclared_sandboxes
                .iter()
                .map(|(step, sb)| format!("step '{step}' → sandbox '{sb}'"))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(WorkflowError::UndeclaredSandbox(format!(
                "{listed}. Add each name to the workflow's top-level `sandboxes` declarations"
            )));
        }
        Ok(())
    }

    /// For every `Preexisting` decl, look the sandbox up by name in
    /// the workflow's project (project-unique) and verify `sub`
    /// can `Read` it. Returns a map from decl name to resolved
    /// `SandboxId` so the skill lookup can fall back to its exports.
    async fn resolve_preexisting_sandboxes(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
        sandboxes: &[WorkflowSandboxDecl],
    ) -> Result<std::collections::HashMap<String, SandboxId>, WorkflowError> {
        let mut out = std::collections::HashMap::new();
        for decl in sandboxes {
            let WorkflowSandboxDecl::Preexisting { name } = decl else {
                continue;
            };
            let sb = self
                .skills
                .sandboxes()
                .find_by_name_in_project(sub, project_id, name)
                .await
                .map_err(|e| {
                    WorkflowError::SandboxNotFound(format!("preexisting sandbox '{name}': {e}"))
                })?;
            out.insert(name.clone(), sb.id);
        }
        Ok(out)
    }

    /// `project_name` is cached on the entity so the forward-sync
    /// hook can render the library path without an extra lookup.
    #[allow(clippy::too_many_arguments)]
    #[instrument(name = "core.workflow.create", skip_all)]
    pub async fn create(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
        project_name: &str,
        name: String,
        description: Option<String>,
        trigger: WorkflowTrigger,
        steps: Vec<WorkflowStepDef>,
        sandboxes: Vec<WorkflowSandboxDecl>,
    ) -> Result<WorkflowDefinition, WorkflowError> {
        sub.can(AuthVerb::Create, AuthResource::Workflow(project_id, None))?;

        if steps.is_empty() {
            return Err(WorkflowError::InvalidDefinition(
                "workflow requires at least one step".into(),
            ));
        }

        Self::validate_trigger(&trigger)?;

        self.validate_steps(sub, project_id, &steps, &sandboxes)
            .await?;

        let trigger = match trigger {
            WorkflowTrigger::Webhook { provider, secret } if secret.is_empty() => {
                WorkflowTrigger::Webhook {
                    provider,
                    secret: Self::generate_webhook_secret(),
                }
            }
            other => other,
        };

        let mut builder = NewWorkflowDefinition::builder()
            .project_id(project_id)
            .name(name)
            .trigger(trigger)
            .steps(steps)
            .sandboxes(sandboxes);
        if !project_name.is_empty() {
            builder = builder.project_name(project_name);
        }
        if let Some(desc) = description {
            builder = builder.description(desc);
        }
        let new = builder
            .build()
            .map_err(|e| WorkflowError::BuildEntity(e.to_string()))?;

        let mut op = self.repo.begin_op().await?;
        self.users.commit_attribution().await;
        let workflow = self.repo.create_in_op(&mut op, new).await?;
        self.register_cron_in_op(&mut op, &workflow).await?;
        op.commit().await?;
        Ok(workflow)
    }

    /// Updates name / description / trigger / steps / sandboxes on
    /// an existing definition. Any `Some` field is applied; `None`
    /// leaves the field unchanged. Steps/sandboxes are re-validated
    /// when supplied; trigger is re-validated when supplied.
    #[allow(clippy::too_many_arguments)]
    #[instrument(name = "core.workflow.update", skip_all)]
    pub async fn update(
        &self,
        sub: &AuthSubject,
        id: WorkflowDefinitionId,
        name: Option<String>,
        description: Option<Option<String>>,
        trigger: Option<WorkflowTrigger>,
        steps: Option<Vec<WorkflowStepDef>>,
        sandboxes: Option<Vec<WorkflowSandboxDecl>>,
    ) -> Result<WorkflowDefinition, WorkflowError> {
        let mut definition = self.repo.find_by_id(id).await?;
        sub.can(
            AuthVerb::Update,
            AuthResource::Workflow(definition.project_id, Some(definition.id)),
        )?;

        if let Some(t) = trigger.as_ref() {
            Self::validate_trigger(t)?;
        }

        let next_steps = steps.as_ref().unwrap_or(&definition.steps);
        let next_sandboxes = sandboxes.as_ref().unwrap_or(&definition.sandboxes);
        if next_steps.is_empty() {
            return Err(WorkflowError::InvalidDefinition(
                "workflow requires at least one step".into(),
            ));
        }
        if steps.is_some() || sandboxes.is_some() {
            self.validate_steps(sub, definition.project_id, next_steps, next_sandboxes)
                .await?;
        }

        // Capture before `update_content` mutates `definition.trigger`.
        // Only a non-cron → cron transition needs a fresh spawn. A
        // cron → cron schedule change is picked up by the in-flight
        // job's next fire (it re-reads the definition); cron → other
        // is handled by the runner exiting when the trigger is no
        // longer `Cron`. Re-spawning in either of those cases
        // duplicates the chain.
        let was_cron = matches!(definition.trigger, WorkflowTrigger::Cron { .. });
        if definition
            .update_content(name, description, trigger, steps, sandboxes)
            .did_execute()
        {
            let mut op = self.repo.begin_op().await?;
            self.users.commit_attribution().await;
            self.repo.update_in_op(&mut op, &mut definition).await?;
            let is_cron = matches!(definition.trigger, WorkflowTrigger::Cron { .. });
            if !was_cron && is_cron {
                self.register_cron_in_op(&mut op, &definition).await?;
            }
            op.commit().await?;
        }
        Ok(definition)
    }

    /// Spawn the next cron fire in the same atomic op as the
    /// workflow create/update. Either both land or both roll back, so
    /// no startup-recovery sweep is needed: the schedule is durably
    /// queued whenever its definition exists. No-op for non-cron
    /// triggers.
    async fn register_cron_in_op<OP: es_entity::AtomicOperation>(
        &self,
        op: &mut OP,
        workflow: &WorkflowDefinition,
    ) -> Result<(), WorkflowError> {
        let WorkflowTrigger::Cron { schedule, timezone } = &workflow.trigger else {
            return Ok(());
        };
        let next_at = match next_cron_fire_at(schedule, timezone.as_deref(), chrono::Utc::now())
            .map_err(WorkflowError::InvalidCronExpression)?
        {
            Some(t) => t,
            None => {
                tracing::warn!(
                    workflow_id = %workflow.id,
                    %schedule,
                    "cron expression has no future fires; skipping registration"
                );
                return Ok(());
            }
        };
        self.cron_spawner
            .spawn_at_with_queue_id_in_op(
                op,
                ::job::JobId::new(),
                TriggerCronConfig {
                    definition_id: workflow.id,
                },
                next_at,
                cron_queue_id(workflow.id),
            )
            .await
            .map_err(|e| WorkflowError::Job(e.to_string()))?;
        Ok(())
    }

    #[instrument(name = "core.workflow.find_by_id", skip_all)]
    pub async fn find_by_id(
        &self,
        sub: &AuthSubject,
        id: WorkflowDefinitionId,
    ) -> Result<WorkflowDefinition, WorkflowError> {
        let workflow = self.repo.find_by_id(id).await?;
        sub.can(
            AuthVerb::Read,
            AuthResource::Workflow(workflow.project_id, Some(workflow.id)),
        )?;
        Ok(workflow)
    }

    /// Bypasses auth — internal callers (the webhook handler) verify
    /// via the trigger's stored secret instead.
    #[instrument(name = "core.workflow.find_by_id_unchecked", skip_all)]
    pub async fn find_by_id_unchecked(
        &self,
        id: WorkflowDefinitionId,
    ) -> Result<WorkflowDefinition, WorkflowError> {
        Ok(self.repo.find_by_id(id).await?)
    }

    #[instrument(name = "core.workflow.list_for_project", skip_all)]
    pub async fn list_for_project(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
    ) -> Result<Vec<WorkflowDefinition>, WorkflowError> {
        sub.can(AuthVerb::Read, AuthResource::Workflow(project_id, None))?;
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
        Ok(result.entities)
    }

    /// Spawns the executor as a job and returns the run synchronously.
    #[instrument(name = "core.workflow.trigger_run", skip_all)]
    pub async fn trigger_run(
        &self,
        sub: &AuthSubject,
        definition_id: WorkflowDefinitionId,
        trigger_context: serde_json::Value,
    ) -> Result<WorkflowRun, WorkflowError> {
        let definition = self.repo.find_by_id(definition_id).await?;
        sub.can(
            AuthVerb::Use,
            AuthResource::Workflow(definition.project_id, Some(definition.id)),
        )?;
        self.spawn_run(definition, trigger_context).await
    }

    /// No auth check — pairs with [`Self::find_by_id_unchecked`] so
    /// the webhook handler doesn't double-load the definition.
    #[instrument(name = "core.workflow.trigger_run_for_definition", skip_all)]
    pub async fn trigger_run_for_definition(
        &self,
        definition: WorkflowDefinition,
        trigger_context: serde_json::Value,
    ) -> Result<WorkflowRun, WorkflowError> {
        self.spawn_run(definition, trigger_context).await
    }

    async fn spawn_run(
        &self,
        definition: WorkflowDefinition,
        trigger_context: serde_json::Value,
    ) -> Result<WorkflowRun, WorkflowError> {
        Self::spawn_run_static(
            &self.run_repo,
            &self.execute_run_spawner,
            definition,
            trigger_context,
        )
        .await
    }

    pub(crate) async fn spawn_run_static(
        run_repo: &WorkflowRunRepo,
        execute_run_spawner: &::job::JobSpawner<ExecuteRunConfig>,
        definition: WorkflowDefinition,
        trigger_context: serde_json::Value,
    ) -> Result<WorkflowRun, WorkflowError> {
        let new = NewWorkflowRun::builder()
            .definition_id(definition.id)
            .project_id(definition.project_id)
            .trigger_context(trigger_context)
            .steps_snapshot(definition.steps.clone())
            .build()
            .map_err(|e| WorkflowError::BuildEntity(e.to_string()))?;

        let run = run_repo.create(new).await?;

        let queue_id = format!("workflow:{}", definition.id);
        execute_run_spawner
            .spawn_with_queue_id(run.id, ExecuteRunConfig { run_id: run.id }, &queue_id)
            .await
            .map_err(|e| WorkflowError::Job(e.to_string()))?;

        Ok(run)
    }

    #[instrument(name = "core.workflow.list_runs", skip_all)]
    pub async fn list_runs(
        &self,
        sub: &AuthSubject,
        definition_id: WorkflowDefinitionId,
    ) -> Result<Vec<WorkflowRun>, WorkflowError> {
        self.list_runs_filtered(sub, definition_id, None, None, None)
            .await
    }

    /// Filtered variant for the `runs` MCP command. `state` filters
    /// the returned set; `before` drops runs whose `started_at` is at
    /// or after the cursor (RFC3339); `limit` caps the final page.
    /// Defaults match the memo: `limit=20`. Returns runs newest-first.
    #[instrument(name = "core.workflow.list_runs_filtered", skip_all)]
    pub async fn list_runs_filtered(
        &self,
        sub: &AuthSubject,
        definition_id: WorkflowDefinitionId,
        state: Option<RunStateFilter>,
        before: Option<chrono::DateTime<chrono::Utc>>,
        limit: Option<usize>,
    ) -> Result<Vec<WorkflowRun>, WorkflowError> {
        let definition = self.repo.find_by_id(definition_id).await?;
        sub.can(
            AuthVerb::Read,
            AuthResource::Workflow(definition.project_id, Some(definition.id)),
        )?;
        // Pull a wide page from the repo and post-filter — runs are
        // bounded per-definition and the memo's `state` + `before`
        // filters are auxiliary, not the primary access path.
        let query = es_entity::PaginatedQueryArgs {
            first: 200,
            after: None,
        };
        let result = self
            .run_repo
            .list_for_definition_id_by_created_at(
                definition_id,
                query,
                es_entity::ListDirection::Descending,
            )
            .await?;

        let cap = limit.unwrap_or(20);
        let filtered: Vec<WorkflowRun> = result
            .entities
            .into_iter()
            .filter(|r| state.as_ref().is_none_or(|s| s.matches(r.state)))
            .filter(|r| before.is_none_or(|cutoff| r.started_at() < cutoff))
            .take(cap)
            .collect();
        Ok(filtered)
    }

    #[instrument(name = "core.workflow.find_run_by_id", skip_all)]
    pub async fn find_run_by_id(
        &self,
        sub: &AuthSubject,
        run_id: WorkflowRunId,
    ) -> Result<WorkflowRun, WorkflowError> {
        let run = self.run_repo.find_by_id(run_id).await?;
        sub.can(
            AuthVerb::Read,
            AuthResource::Workflow(run.project_id, Some(run.definition_id)),
        )?;
        Ok(run)
    }

    /// Returns the run plus per-step telemetry (one entry per
    /// `step_results[i]`). Telemetry is `None` when the step never
    /// spawned an agent (e.g. it failed before agent creation, or the
    /// run is still pending). Agent / session reads piggy-back on the
    /// caller's workflow read scope — both live in the same project.
    #[instrument(name = "core.workflow.find_run_with_step_details", skip_all)]
    pub async fn find_run_with_step_details(
        &self,
        sub: &AuthSubject,
        run_id: WorkflowRunId,
    ) -> Result<RunWithStepDetails, WorkflowError> {
        let run = self.find_run_by_id(sub, run_id).await?;
        let agents = self
            .agents
            .list_for_workflow_run(sub, run.project_id, run.id)
            .await
            .map_err(|e| WorkflowError::Agent(e.to_string()))?;

        let prefix = workflow_agent_name_prefix(run.id);

        let mut steps = Vec::with_capacity(run.step_results.len());
        for sr in &run.step_results {
            let agent = agents
                .iter()
                .find(|a| matches_step_agent(&a.name, &prefix, &sr.name));
            let telemetry = match agent {
                Some(a) => match self.agents.find_session(sub, a.id).await {
                    Ok(session) => Some(StepAgentDetails {
                        agent_id: a.id,
                        session_id: session.id,
                        summary: session.tool_call_summary(),
                    }),
                    Err(_) => None,
                },
                None => None,
            };
            steps.push(StepWithDetails {
                step_name: sr.name.clone(),
                error: sr.error.clone(),
                completed_at: sr.completed_at,
                output: sr.output.clone(),
                details: telemetry,
            });
        }

        Ok(RunWithStepDetails { run, steps })
    }

    /// Block until the run reaches a terminal state, then return it.
    /// Backed by `Jobs::await_completions` on the `ExecuteRun` job —
    /// the spawner uses the run id as the job id (see `spawn_run`).
    /// Returns immediately if the run is already terminal.
    #[instrument(name = "core.workflow.await_run_completion", skip_all)]
    pub async fn await_run_completion(
        &self,
        sub: &AuthSubject,
        run_id: WorkflowRunId,
        timeout: Option<std::time::Duration>,
    ) -> Result<WorkflowRun, WorkflowError> {
        let run = self.run_repo.find_by_id(run_id).await?;
        sub.can(
            AuthVerb::Read,
            AuthResource::Workflow(run.project_id, Some(run.definition_id)),
        )?;
        if matches!(
            run.state,
            WorkflowRunState::Succeeded | WorkflowRunState::Failed
        ) {
            return Ok(run);
        }
        self.jobs
            .await_completions(&[run_id.into()], timeout)
            .await
            .map_err(|e| WorkflowError::Job(e.to_string()))?;
        Ok(self.run_repo.find_by_id(run_id).await?)
    }

    #[instrument(name = "core.workflow.delete_for_project_in_op", skip_all)]
    pub(crate) async fn delete_for_project_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        project_id: ProjectId,
    ) -> Result<(), WorkflowError> {
        // Runs FK-reference definitions, so delete runs first.
        self.run_repo
            .cascade_delete_for_project_in_op(op, project_id)
            .await?;
        self.repo
            .cascade_delete_for_project_in_op(op, project_id)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_trigger_accepts_manual_and_webhook() {
        Workflows::validate_trigger(&WorkflowTrigger::Manual).unwrap();
        Workflows::validate_trigger(&WorkflowTrigger::Webhook {
            provider: Some("honeycomb".into()),
            secret: "whsec".into(),
        })
        .unwrap();
    }

    #[test]
    fn validate_trigger_accepts_valid_cron() {
        Workflows::validate_trigger(&WorkflowTrigger::Cron {
            schedule: "0 */6 * * * *".into(),
            timezone: Some("UTC".into()),
        })
        .unwrap();
    }

    #[test]
    fn validate_trigger_rejects_bad_cron_expression() {
        let err = Workflows::validate_trigger(&WorkflowTrigger::Cron {
            schedule: "not a cron".into(),
            timezone: None,
        })
        .unwrap_err();
        assert!(matches!(err, WorkflowError::InvalidCronExpression(_)));
    }

    #[test]
    fn run_state_filter_matches_each_state() {
        assert!(RunStateFilter::Succeeded.matches(WorkflowRunState::Succeeded));
        assert!(!RunStateFilter::Succeeded.matches(WorkflowRunState::Failed));
        assert!(RunStateFilter::Failed.matches(WorkflowRunState::Failed));
        assert!(RunStateFilter::Running.matches(WorkflowRunState::Pending));
        assert!(RunStateFilter::Running.matches(WorkflowRunState::Running));
        assert!(!RunStateFilter::Running.matches(WorkflowRunState::Succeeded));
    }

    #[test]
    fn matches_step_agent_strips_run_prefix_and_compares_step_name() {
        // executor.rs builds names as "workflow-{short}-{step_name}".
        let prefix = "workflow-019df6b8-".to_string();
        let agent_name = format!("{prefix}classify-and-comment");
        assert!(super::matches_step_agent(
            &agent_name,
            &prefix,
            "classify-and-comment"
        ));
        assert!(!super::matches_step_agent(
            &agent_name,
            &prefix,
            "different-step"
        ));
        let other_prefix = "workflow-019df6b3-".to_string();
        assert!(!super::matches_step_agent(
            &agent_name,
            &other_prefix,
            "classify-and-comment"
        ));
        // Step names with embedded hyphens still match when the suffix is exact.
        let with_hyphens = format!("{prefix}step-with-hyphens");
        assert!(super::matches_step_agent(
            &with_hyphens,
            &prefix,
            "step-with-hyphens"
        ));
    }

    #[test]
    fn workflow_agent_name_prefix_uses_first_uuid_segment() {
        let run_id = WorkflowRunId::new();
        let prefix = workflow_agent_name_prefix(run_id);
        let s = run_id.to_string();
        let short = s.split_once('-').unwrap().0;
        assert_eq!(prefix, format!("workflow-{short}-"));
    }

    #[test]
    fn validate_trigger_rejects_bad_timezone() {
        let err = Workflows::validate_trigger(&WorkflowTrigger::Cron {
            schedule: "0 */6 * * * *".into(),
            timezone: Some("Mars/Olympus_Mons".into()),
        })
        .unwrap_err();
        assert!(matches!(err, WorkflowError::InvalidTimezone(_)));
    }
}
