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
use crate::library::{GitFileHash, Library};
use crate::primitives::*;
use crate::sandbox::Sandboxes;
use crate::skill::Skills;

pub use definition::{WorkflowSandboxDecl, WorkflowStepDef, WorkflowTrigger};
pub use entity::*;
pub use error::*;
pub use job::ExecuteRunJobInitializer;
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

impl crate::library::LibraryImporter for Workflows {
    type Entity = WorkflowDefinition;
    const JOB_TYPE: &'static str = "workflow.sync-from-library";
    const WORKSPACE_REQUIRED: bool = true;

    fn parse(content: &str, path: &str) -> Option<crate::library::ParsedFile> {
        crate::library::parse_workflow_yaml(content, path).map(|p| p.parsed)
    }

    /// On create: mints a fresh webhook secret. On update: preserves the
    /// DB-side secret; the file never carries it. Re-parses the canonical
    /// YAML payload to recover format-specific fields
    /// (trigger/steps/sandboxes) that `SyncedFile` does not carry.
    #[instrument(name = "core.workflow.library_importer.upsert_in_op", skip_all)]
    async fn upsert_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        file: &crate::library::SyncedFile,
        workspace_id: Option<WorkspaceId>,
        file_hash: GitFileHash,
    ) -> Result<(), crate::library::UpsertError> {
        let workspace_id = workspace_id.ok_or_else(|| -> crate::library::UpsertError {
            "workflow upsert requires a workspace id".into()
        })?;

        if file.doc_type != crate::library::DocType::Workflow {
            return Ok(());
        }

        let synthetic_path = file
            .original_path
            .clone()
            .unwrap_or_else(|| file.relative_path());
        let parsed = crate::library::parse_workflow_yaml(&file.rendered, &synthetic_path)
            .ok_or_else(|| {
                WorkflowError::BuildEntity("workflow YAML failed to round-trip".into())
            })?;

        let doc_id = WorkflowDefinitionId::from(file.doc_id);
        let name = file.title.clone();
        let description = parsed.description;
        let trigger = parsed.trigger;
        let steps = parsed.steps;
        let sandboxes = parsed.sandboxes;
        let workspace_name = file.workspace_name.clone();
        let original_path = file.original_path.clone();

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
    /// against the workflow's own declarations. Workspace skills win
    /// over sandbox-exported skills; an agent step backed by a
    /// `Preexisting` sandbox may also resolve its skill from that
    /// sandbox's exported set.
    async fn validate_steps(
        &self,
        sub: &AuthSubject,
        workspace_id: WorkspaceId,
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
        // sandbox in the workspace + auth-check Read. The map is
        // consulted per-step so a sandbox-only skill counts as
        // resolved when its step targets a Preexisting sandbox.
        let preexisting_ids = self
            .resolve_preexisting_sandboxes(sub, workspace_id, sandboxes)
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
                    let found = self
                        .skills
                        .find_by_name(skill, Some(workspace_id), preexisting_sandbox_id)
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
                "{listed}. The `skill` field expects the NAME of a skill in this workspace; create skills first via the `skill` tool"
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
    /// the workflow's workspace (workspace-unique) and verify `sub`
    /// can `Read` it. Returns a map from decl name to resolved
    /// `SandboxId` so the skill lookup can fall back to its exports.
    async fn resolve_preexisting_sandboxes(
        &self,
        sub: &AuthSubject,
        workspace_id: WorkspaceId,
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
                .find_by_name_in_workspace(sub, workspace_id, name)
                .await
                .map_err(|e| {
                    WorkflowError::SandboxNotFound(format!("preexisting sandbox '{name}': {e}"))
                })?;
            out.insert(name.clone(), sb.id);
        }
        Ok(out)
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

        self.validate_steps(sub, workspace_id, &steps, &sandboxes)
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
