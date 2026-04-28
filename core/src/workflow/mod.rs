pub mod definition;
pub mod entity;
pub mod error;
pub mod executor;
pub(crate) mod job;
pub(crate) mod repo;
pub mod run;

use std::sync::Arc;

use rand::RngCore;
use tracing::instrument;

use crate::agent::Agents;
use crate::library::{GitFileHash, Library, RuntimeFile};
use crate::primitives::*;
use crate::sandbox::Sandboxes;
use crate::skill::Skills;

// `agents` / `skills` / `sandboxes` are owned by the
// `workflow.execute-run` job runner (see `Workflows::execute_run_job_initializer`),
// not by `Workflows` directly — the service only persists the run and
// enqueues the job, the runner does the actual work.

pub use definition::{WorkflowStepDef, WorkflowTrigger};
pub use entity::*;
pub use error::*;
pub use job::{
    ExecuteRunJobInitializer, SyncWorkflowsFromLibraryConfig,
    SyncWorkflowsFromLibraryJobInitializer,
};
pub use run::{StepResult, WorkflowRun, WorkflowRunRepo, WorkflowRunState};

use job::ExecuteRunConfig;
use repo::WorkflowDefinitionRepo;
use run::entity::NewWorkflowRun;

#[derive(Clone)]
pub struct Workflows {
    repo: WorkflowDefinitionRepo,
    run_repo: WorkflowRunRepo,
    /// Used to validate `skill:` references on `create` so the operator
    /// gets immediate feedback instead of a runtime `SkillNotFound`.
    skills: Arc<Skills>,
    /// Same idea for `sandbox:` references.
    sandboxes: Arc<Sandboxes>,
    /// Job spawner for the `workflow.execute-run` background job — wired
    /// up at [`crate::App::init`] time after the job system is online.
    execute_run_spawner: ::job::JobSpawner<ExecuteRunConfig>,
}

impl Workflows {
    pub fn new(
        pool: &sqlx::PgPool,
        library: Library,
        skills: Arc<Skills>,
        sandboxes: Arc<Sandboxes>,
        execute_run_spawner: ::job::JobSpawner<ExecuteRunConfig>,
    ) -> Self {
        Self {
            repo: WorkflowDefinitionRepo::new(pool, library),
            run_repo: WorkflowRunRepo::new(pool),
            skills,
            sandboxes,
            execute_run_spawner,
        }
    }

    /// Test/dev constructor — bypasses library wiring (no git sync).
    pub fn new_without_library(
        pool: &sqlx::PgPool,
        skills: Arc<Skills>,
        sandboxes: Arc<Sandboxes>,
        execute_run_spawner: ::job::JobSpawner<ExecuteRunConfig>,
    ) -> Self {
        Self {
            repo: WorkflowDefinitionRepo::new_without_library(pool),
            run_repo: WorkflowRunRepo::new(pool),
            skills,
            sandboxes,
            execute_run_spawner,
        }
    }

    /// Job initializer for `workflow.execute-run`. Build this at App
    /// startup, register it with `Jobs::add_initializer`, and pass the
    /// returned spawner to [`Workflows::new`].
    pub fn execute_run_job_initializer(
        pool: &sqlx::PgPool,
        agents: Arc<Agents>,
        skills: Arc<Skills>,
        sandboxes: Arc<Sandboxes>,
    ) -> ExecuteRunJobInitializer {
        ExecuteRunJobInitializer::new(WorkflowRunRepo::new(pool), agents, skills, sandboxes)
    }

    /// Auto-generate a webhook secret of the form `whsec_<32 hex chars>`.
    fn generate_webhook_secret() -> String {
        let mut bytes = [0u8; 16];
        rand::rng().fill_bytes(&mut bytes);
        format!("whsec_{}", hex::encode(bytes))
    }

    /// Verify every step's `skill:` resolves to a real skill in the
    /// workspace (or globally), and every `sandbox:` name resolves to
    /// an existing workspace sandbox. Surfaces
    /// [`WorkflowError::SkillNotFound`] / [`WorkflowError::SandboxNotFound`]
    /// at create time so the operator gets immediate feedback instead
    /// of a runtime failure deep inside the executor.
    async fn validate_steps(
        &self,
        sub: &AuthSubject,
        workspace_id: WorkspaceId,
        steps: &[WorkflowStepDef],
    ) -> Result<(), WorkflowError> {
        // Pre-fetch the workspace's sandboxes once so every step shares the
        // lookup.
        let sandbox_names: Vec<String> = self
            .sandboxes
            .list_for_workspace(sub, workspace_id)
            .await
            .map_err(|e| WorkflowError::Sandbox(e.to_string()))?
            .into_iter()
            .map(|s| s.name)
            .collect();

        for step in steps {
            match step {
                WorkflowStepDef::AgentStep { skill, sandbox, .. } => {
                    // We don't know yet which sandbox each step will
                    // resolve to (sandbox skills are scoped per
                    // attachment), so check against workspace + global
                    // skills only — exactly the lookup `Workflows::create`
                    // controls. Sandbox-exported skills can still
                    // resolve at runtime.
                    let found = self
                        .skills
                        .find_by_name(skill, Some(workspace_id), None)
                        .await
                        .map_err(|e| WorkflowError::Skill(e.to_string()))?;
                    if found.is_none() {
                        return Err(WorkflowError::SkillNotFound(skill.clone()));
                    }

                    if let Some(sandbox_name) = sandbox {
                        if !sandbox_names.iter().any(|n| n == sandbox_name) {
                            return Err(WorkflowError::SandboxNotFound(sandbox_name.clone()));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Create a new workflow definition.
    ///
    /// `workspace_name` is the cached workspace display name — needed
    /// so the post-persist hook can render the canonical library file
    /// path without an extra DB lookup. Pass an empty string for tests
    /// that don't care about library-side naming (the file just won't
    /// land under a workspace folder).
    #[allow(clippy::too_many_arguments)]
    #[instrument(name = "core.workflow.create", skip_all)]
    pub async fn create(
        &self,
        sub: &AuthSubject,
        workspace_id: WorkspaceId,
        workspace_name: &str,
        name: String,
        description: Option<String>,
        trigger: WorkflowTrigger,
        steps: Vec<WorkflowStepDef>,
    ) -> Result<WorkflowDefinition, WorkflowError> {
        sub.can(AuthVerb::Create, AuthResource::Workflow(workspace_id, None))?;

        if steps.is_empty() {
            return Err(WorkflowError::InvalidDefinition(
                "workflow requires at least one step".into(),
            ));
        }

        self.validate_steps(sub, workspace_id, &steps).await?;

        // Auto-generate the webhook secret if the trigger is webhook +
        // the caller passed an empty/placeholder secret.
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
            .workspace_id(workspace_id)
            .name(name)
            .trigger(trigger)
            .steps(steps);
        if !workspace_name.is_empty() {
            builder = builder.workspace_name(workspace_name);
        }
        if let Some(desc) = description {
            builder = builder.description(desc);
        }
        let new = builder
            .build()
            .map_err(|e| WorkflowError::BuildEntity(e.to_string()))?;

        let workflow = self.repo.create(new).await?;
        Ok(workflow)
    }

    /// Reverse-sync entry point: upsert a workflow from a library file
    /// within an existing transaction. Mirrors `Skills::upsert_from_library_in_op`.
    ///
    /// On `Create`: stamps `original_path` so the next forward-sync
    /// pass can rename / clean up the file. Generates a fresh webhook
    /// secret when the file declares a webhook trigger.
    /// On `Update`: preserves the existing DB-side webhook secret —
    /// secrets never round-trip through the file.
    #[instrument(name = "core.workflow.upsert_from_library_in_op", skip_all)]
    pub(crate) async fn upsert_from_library_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        file: &RuntimeFile,
        workspace_id: WorkspaceId,
        file_hash: GitFileHash,
    ) -> Result<(), WorkflowError> {
        let (doc_id, name, description, trigger, steps, workspace_name, original_path) = match file
        {
            RuntimeFile::Workflow {
                doc_id,
                name,
                description,
                trigger,
                steps,
                workspace_name,
                original_path,
                ..
            } => (
                *doc_id,
                name.clone(),
                description.clone(),
                trigger.clone(),
                steps.clone(),
                workspace_name.clone(),
                original_path.clone(),
            ),
            _ => return Ok(()),
        };

        // Soft validation on reverse-sync: warn but don't fail. The
        // library may legitimately be in an intermediate state during
        // a multi-file push (workflow file landing before its
        // referenced skill).
        for step in &steps {
            match step {
                WorkflowStepDef::AgentStep { skill, .. } => {
                    if let Ok(None) = self
                        .skills
                        .find_by_name(skill, Some(workspace_id), None)
                        .await
                    {
                        tracing::warn!(
                            workflow = %name,
                            skill = %skill,
                            "workflow references unknown skill (will fail at run time if not added before triggering)"
                        );
                    }
                }
            }
        }

        if let Some(mut existing) = self.repo.maybe_find_by_id(doc_id).await? {
            if existing
                .update_from_library(
                    Some(name.clone()),
                    Some(description.clone()),
                    Some(trigger),
                    Some(steps),
                    file_hash,
                )
                .did_execute()
            {
                self.repo.update_in_op(op, &mut existing).await?;
            }
            tracing::info!(id = %doc_id, name = %name, "updated workflow from library");
        } else {
            // New workflow from a hand-authored file: mint a fresh secret
            // for webhook triggers.
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
                .id(doc_id)
                .workspace_id(workspace_id)
                .name(name.clone())
                .trigger(trigger)
                .steps(steps);
            if let Some(ws) = workspace_name {
                builder = builder.workspace_name(ws);
            }
            if let Some(desc) = description {
                builder = builder.description(desc);
            }
            if let Some(path) = original_path {
                builder = builder.original_path(path);
            }
            let new = builder
                .build()
                .map_err(|e| WorkflowError::BuildEntity(e.to_string()))?;
            self.repo.create_in_op(op, new).await?;
            tracing::info!(id = %doc_id, name = %name, "created workflow from library");
        }
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
            AuthResource::Workflow(workflow.workspace_id, Some(workflow.id)),
        )?;
        Ok(workflow)
    }

    /// Look up a workflow without performing an auth check.
    ///
    /// Reserved for internal callers (the webhook handler in particular)
    /// that authenticate via the trigger's stored secret rather than via
    /// [`AuthSubject`]. The caller is expected to use the returned
    /// definition's stored secret to verify the request before invoking
    /// [`Self::trigger_run_for_definition`] with the same value.
    #[instrument(name = "core.workflow.find_by_id_unchecked", skip_all)]
    pub async fn find_by_id_unchecked(
        &self,
        id: WorkflowDefinitionId,
    ) -> Result<WorkflowDefinition, WorkflowError> {
        Ok(self.repo.find_by_id(id).await?)
    }

    #[instrument(name = "core.workflow.list_for_workspace", skip_all)]
    pub async fn list_for_workspace(
        &self,
        sub: &AuthSubject,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<WorkflowDefinition>, WorkflowError> {
        sub.can(AuthVerb::Read, AuthResource::Workflow(workspace_id, None))?;
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
        Ok(result.entities)
    }

    /// Create a [`WorkflowRun`] for `definition_id`, persist it, and spawn
    /// the executor on a tokio task. Returns the freshly-created run
    /// immediately — the executor runs in the background and updates the
    /// run as steps progress.
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
            AuthResource::Workflow(definition.workspace_id, Some(definition.id)),
        )?;
        self.spawn_run(definition, trigger_context).await
    }

    /// Spawn a run for a pre-loaded definition without an auth check.
    ///
    /// Pairs with [`Self::find_by_id_unchecked`] so the webhook handler
    /// can load the definition once (to read the trigger secret), verify
    /// the request, then trigger the run without re-fetching.
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
        let new = NewWorkflowRun::builder()
            .definition_id(definition.id)
            .workspace_id(definition.workspace_id)
            .trigger_context(trigger_context)
            .steps_snapshot(definition.steps.clone())
            .build()
            .map_err(|e| WorkflowError::BuildEntity(e.to_string()))?;

        let run = self.run_repo.create(new).await?;

        // Enqueue the `workflow.execute-run` job. The job system
        // persists the request in PostgreSQL, so even if the process
        // crashes between this call and the runner picking it up the
        // executor will still run after restart. Combined with the
        // idempotent mutations on [`WorkflowRun`], this gives
        // at-least-once execution semantics that survive deploys.
        self.execute_run_spawner
            .spawn(::job::JobId::new(), ExecuteRunConfig { run_id: run.id })
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
        let definition = self.repo.find_by_id(definition_id).await?;
        sub.can(
            AuthVerb::Read,
            AuthResource::Workflow(definition.workspace_id, Some(definition.id)),
        )?;
        let query = es_entity::PaginatedQueryArgs {
            first: 100,
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
        Ok(result.entities)
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
            AuthResource::Workflow(run.workspace_id, Some(run.definition_id)),
        )?;
        Ok(run)
    }
}
