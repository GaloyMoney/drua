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

pub use definition::{WorkflowSandboxDecl, WorkflowStepDef, WorkflowTrigger};
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
    skills: Arc<Skills>,
    execute_run_spawner: ::job::JobSpawner<ExecuteRunConfig>,
}

impl Workflows {
    pub fn new(
        pool: &sqlx::PgPool,
        library: Library,
        skills: Arc<Skills>,
        execute_run_spawner: ::job::JobSpawner<ExecuteRunConfig>,
    ) -> Self {
        Self {
            repo: WorkflowDefinitionRepo::new(pool, library),
            run_repo: WorkflowRunRepo::new(pool),
            skills,
            execute_run_spawner,
        }
    }

    /// No git sync — for tests.
    pub fn new_without_library(
        pool: &sqlx::PgPool,
        skills: Arc<Skills>,
        execute_run_spawner: ::job::JobSpawner<ExecuteRunConfig>,
    ) -> Self {
        Self {
            repo: WorkflowDefinitionRepo::new_without_library(pool),
            run_repo: WorkflowRunRepo::new(pool),
            skills,
            execute_run_spawner,
        }
    }

    pub fn execute_run_job_initializer(
        pool: &sqlx::PgPool,
        agents: Arc<Agents>,
        skills: Arc<Skills>,
        sandboxes: Arc<Sandboxes>,
    ) -> ExecuteRunJobInitializer {
        ExecuteRunJobInitializer::new(
            WorkflowRunRepo::new(pool),
            WorkflowDefinitionRepo::new_without_library(pool),
            agents,
            skills,
            sandboxes,
        )
    }

    fn generate_webhook_secret() -> String {
        let mut bytes = [0u8; 16];
        rand::rng().fill_bytes(&mut bytes);
        format!("whsec_{}", hex::encode(bytes))
    }

    /// Resolve `skill:` references and validate sandbox references
    /// against the workflow's own declarations. Skill resolution skips
    /// sandbox-exported skills — those resolve per attachment at run time.
    async fn validate_steps(
        &self,
        workspace_id: WorkspaceId,
        steps: &[WorkflowStepDef],
        sandboxes: &[WorkflowSandboxDecl],
    ) -> Result<(), WorkflowError> {
        let mut decl_names: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for decl in sandboxes {
            if !decl_names.insert(decl.name.as_str()) {
                return Err(WorkflowError::DuplicateSandboxName(decl.name.clone()));
            }
        }

        for step in steps {
            match step {
                WorkflowStepDef::AgentStep { skill, sandbox, .. } => {
                    let found = self
                        .skills
                        .find_by_name(skill, Some(workspace_id), None)
                        .await
                        .map_err(|e| WorkflowError::Skill(e.to_string()))?;
                    if found.is_none() {
                        return Err(WorkflowError::SkillNotFound(skill.clone()));
                    }

                    if let Some(sandbox_name) = sandbox {
                        if !decl_names.contains(sandbox_name.as_str()) {
                            return Err(WorkflowError::UndeclaredSandbox(sandbox_name.clone()));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// `workspace_name` is cached on the entity so the forward-sync
    /// hook can render the library path without an extra lookup.
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
        sandboxes: Vec<WorkflowSandboxDecl>,
    ) -> Result<WorkflowDefinition, WorkflowError> {
        sub.can(AuthVerb::Create, AuthResource::Workflow(workspace_id, None))?;

        if steps.is_empty() {
            return Err(WorkflowError::InvalidDefinition(
                "workflow requires at least one step".into(),
            ));
        }

        self.validate_steps(workspace_id, &steps, &sandboxes).await?;

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
            .steps(steps)
            .sandboxes(sandboxes);
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

    /// Mirrors `Skills::upsert_from_library_in_op`. On create: mints a
    /// fresh webhook secret. On update: preserves the DB-side secret;
    /// the file never carries it.
    #[instrument(name = "core.workflow.upsert_from_library_in_op", skip_all)]
    pub(crate) async fn upsert_from_library_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        file: &RuntimeFile,
        workspace_id: WorkspaceId,
        file_hash: GitFileHash,
    ) -> Result<(), WorkflowError> {
        let (
            doc_id,
            name,
            description,
            trigger,
            steps,
            sandboxes,
            workspace_name,
            original_path,
        ) = match file {
            RuntimeFile::Workflow {
                doc_id,
                name,
                description,
                trigger,
                steps,
                sandboxes,
                workspace_name,
                original_path,
                ..
            } => (
                *doc_id,
                name.clone(),
                description.clone(),
                trigger.clone(),
                steps.clone(),
                sandboxes.clone(),
                workspace_name.clone(),
                original_path.clone(),
            ),
            _ => return Ok(()),
        };

        // Soft check: a multi-file library push can legitimately land
        // a workflow before its referenced skill, so warn don't fail.
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
                    Some(sandboxes),
                    file_hash,
                )
                .did_execute()
            {
                self.repo.update_in_op(op, &mut existing).await?;
            }
            tracing::info!(id = %doc_id, name = %name, "updated workflow from library");
        } else {
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
                .steps(steps)
                .sandboxes(sandboxes);
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

    /// Bypasses auth — internal callers (the webhook handler) verify
    /// via the trigger's stored secret instead.
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
            AuthResource::Workflow(definition.workspace_id, Some(definition.id)),
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
        let new = NewWorkflowRun::builder()
            .definition_id(definition.id)
            .workspace_id(definition.workspace_id)
            .trigger_context(trigger_context)
            .steps_snapshot(definition.steps.clone())
            .build()
            .map_err(|e| WorkflowError::BuildEntity(e.to_string()))?;

        let run = self.run_repo.create(new).await?;

        let queue_id = format!("workflow:{}", definition.id);
        self.execute_run_spawner
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

    #[instrument(name = "core.workflow.delete_for_workspace_in_op", skip_all)]
    pub(crate) async fn delete_for_workspace_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        workspace_id: WorkspaceId,
    ) -> Result<(), WorkflowError> {
        // Runs FK-reference definitions, so delete runs first.
        self.run_repo
            .cascade_delete_for_workspace_in_op(op, workspace_id)
            .await?;
        self.repo
            .cascade_delete_for_workspace_in_op(op, workspace_id)
            .await?;
        Ok(())
    }
}
