pub mod entity;
pub mod error;
pub mod executor;
pub(crate) mod repo;
pub mod run;

use std::sync::Arc;

use rand::RngCore;
use tracing::instrument;

use crate::agent::Agents;
use crate::primitives::*;
use crate::sandbox::Sandboxes;
use crate::skill::Skills;

pub use entity::*;
pub use error::*;
pub use run::{StepResult, WorkflowRun, WorkflowRunRepo, WorkflowRunState};

use repo::WorkflowDefinitionRepo;
use run::entity::NewWorkflowRun;

#[derive(Clone)]
pub struct Workflows {
    repo: WorkflowDefinitionRepo,
    run_repo: WorkflowRunRepo,
    agents: Arc<Agents>,
    skills: Arc<Skills>,
    sandboxes: Arc<Sandboxes>,
}

impl Workflows {
    pub fn new(
        pool: &sqlx::PgPool,
        agents: Arc<Agents>,
        skills: Arc<Skills>,
        sandboxes: Arc<Sandboxes>,
    ) -> Self {
        Self {
            repo: WorkflowDefinitionRepo::new(pool),
            run_repo: WorkflowRunRepo::new(pool),
            agents,
            skills,
            sandboxes,
        }
    }

    /// Auto-generate a webhook secret of the form `whsec_<32 hex chars>`.
    fn generate_webhook_secret() -> String {
        let mut bytes = [0u8; 16];
        rand::rng().fill_bytes(&mut bytes);
        format!("whsec_{}", hex::encode(bytes))
    }

    #[instrument(name = "core.workflow.create", skip_all)]
    pub async fn create(
        &self,
        sub: &AuthSubject,
        workspace_id: WorkspaceId,
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

        let new = NewWorkflowDefinition::builder()
            .workspace_id(workspace_id)
            .name(name)
            .description(description.unwrap_or_default())
            .trigger(trigger)
            .steps(steps)
            .build()
            .map_err(|e| WorkflowError::BuildEntity(e.to_string()))?;

        let workflow = self.repo.create(new).await?;
        Ok(workflow)
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
        let run_id = run.id;

        let runs = self.run_repo.clone();
        let agents = Arc::clone(&self.agents);
        let skills = Arc::clone(&self.skills);
        let sandboxes = Arc::clone(&self.sandboxes);
        tokio::spawn(async move {
            if let Err(e) = executor::execute_run(runs, agents, skills, sandboxes, run_id).await {
                tracing::error!(error = %e, %run_id, "workflow execution failed to record terminal state");
            }
        });

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
