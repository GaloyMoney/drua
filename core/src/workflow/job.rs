//! Workflow execution jobs.
//!
//! Replaces the original `tokio::spawn` so that workflow runs survive
//! deploys / process restarts. The job system persists `(job_type,
//! config)` to PostgreSQL, retries on failure, and re-runs anything that
//! was in flight when the process died — paired with the idempotent
//! mutations on [`super::run::WorkflowRun`] this gives at-least-once
//! execution semantics.

use std::sync::Arc;

use job::*;
use serde::{Deserialize, Serialize};

use crate::agent::Agents;
use crate::library::{Library, RuntimeFile, LIBRARY_LOCK_QUEUE};
use crate::primitives::WorkflowRunId;
use crate::sandbox::Sandboxes;
use crate::skill::Skills;
use crate::workspace::Workspaces;

use super::executor;
use super::run::WorkflowRunRepo;
use super::Workflows;

pub(crate) const EXECUTE_RUN_JOB: &str = "workflow.execute-run";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteRunConfig {
    pub run_id: WorkflowRunId,
}

pub struct ExecuteRunJobInitializer {
    runs: WorkflowRunRepo,
    agents: Arc<Agents>,
    skills: Arc<Skills>,
    sandboxes: Arc<Sandboxes>,
}

impl ExecuteRunJobInitializer {
    pub fn new(
        runs: WorkflowRunRepo,
        agents: Arc<Agents>,
        skills: Arc<Skills>,
        sandboxes: Arc<Sandboxes>,
    ) -> Self {
        Self {
            runs,
            agents,
            skills,
            sandboxes,
        }
    }
}

impl JobInitializer for ExecuteRunJobInitializer {
    type Config = ExecuteRunConfig;

    fn job_type(&self) -> JobType {
        JobType::new(EXECUTE_RUN_JOB)
    }

    fn init(
        &self,
        job: &Job,
        _spawner: JobSpawner<Self::Config>,
    ) -> Result<Box<dyn JobRunner>, Box<dyn std::error::Error>> {
        let config: ExecuteRunConfig = job.config()?;
        Ok(Box::new(ExecuteRunRunner {
            runs: self.runs.clone(),
            agents: Arc::clone(&self.agents),
            skills: Arc::clone(&self.skills),
            sandboxes: Arc::clone(&self.sandboxes),
            config,
        }))
    }
}

struct ExecuteRunRunner {
    runs: WorkflowRunRepo,
    agents: Arc<Agents>,
    skills: Arc<Skills>,
    sandboxes: Arc<Sandboxes>,
    config: ExecuteRunConfig,
}

#[async_trait::async_trait]
impl JobRunner for ExecuteRunRunner {
    #[tracing::instrument(name = "core.workflow.execute_run.job", skip_all, fields(run_id = %self.config.run_id))]
    async fn run(
        &self,
        _current_job: CurrentJob,
    ) -> Result<JobCompletion, Box<dyn std::error::Error>> {
        executor::execute_run(
            self.runs.clone(),
            Arc::clone(&self.agents),
            Arc::clone(&self.skills),
            Arc::clone(&self.sandboxes),
            self.config.run_id,
        )
        .await?;
        Ok(JobCompletion::Complete)
    }
}

// ─── Reverse-sync job: import workflow YAML files from the library ─────────
//
// Mirrors `skill::job::SyncSkillsFromLibraryJob` — interval-poll the
// library repo, find changed `*.yml` files under `runtime/workflows/`
// or `runtime/workspaces/*/workflows/`, and upsert them into the DB
// inside one transaction. Runs on the `library-lock` queue so it
// serialises with forward-sync (`WriteToRuntime`) on the same repo.

pub(crate) const SYNC_WORKFLOWS_FROM_LIBRARY_JOB: &str = "workflow.sync-from-library";

#[derive(Debug, Serialize, Deserialize)]
pub struct SyncWorkflowsFromLibraryConfig {
    pub sync_interval_secs: u64,
    #[serde(default)]
    #[allow(clippy::option_option)]
    pub last_sync_commit: Option<String>,
}

impl SyncWorkflowsFromLibraryConfig {
    fn next(&self, last_sync_commit: Option<String>) -> Self {
        Self {
            sync_interval_secs: self.sync_interval_secs,
            last_sync_commit,
        }
    }
}

pub struct SyncWorkflowsFromLibraryJobInitializer {
    library: Library,
    workflows: Arc<Workflows>,
    workspaces: Arc<Workspaces>,
}

impl SyncWorkflowsFromLibraryJobInitializer {
    pub fn new(library: Library, workflows: Arc<Workflows>, workspaces: Arc<Workspaces>) -> Self {
        Self {
            library,
            workflows,
            workspaces,
        }
    }
}

impl JobInitializer for SyncWorkflowsFromLibraryJobInitializer {
    type Config = SyncWorkflowsFromLibraryConfig;

    fn job_type(&self) -> JobType {
        JobType::new(SYNC_WORKFLOWS_FROM_LIBRARY_JOB)
    }

    fn init(
        &self,
        job: &Job,
        spawner: JobSpawner<Self::Config>,
    ) -> Result<Box<dyn JobRunner>, Box<dyn std::error::Error>> {
        let config: SyncWorkflowsFromLibraryConfig = job.config()?;
        Ok(Box::new(SyncWorkflowsFromLibraryRunner {
            library: self.library.clone(),
            workflows: Arc::clone(&self.workflows),
            workspaces: Arc::clone(&self.workspaces),
            config,
            spawner,
        }))
    }
}

struct SyncWorkflowsFromLibraryRunner {
    library: Library,
    workflows: Arc<Workflows>,
    workspaces: Arc<Workspaces>,
    config: SyncWorkflowsFromLibraryConfig,
    spawner: JobSpawner<SyncWorkflowsFromLibraryConfig>,
}

#[async_trait::async_trait]
impl JobRunner for SyncWorkflowsFromLibraryRunner {
    #[tracing::instrument(name = "core.workflow.sync_from_library.run", skip_all)]
    async fn run(
        &self,
        current_job: CurrentJob,
    ) -> Result<JobCompletion, Box<dyn std::error::Error>> {
        let next_commit = match self.sync_once(&current_job).await {
            Ok(commit) => commit,
            Err(e) => {
                tracing::warn!(error = %e, "workflow sync cycle failed");
                self.config.last_sync_commit.clone()
            }
        };

        let next_config = self.config.next(next_commit);
        let schedule_at =
            chrono::Utc::now() + chrono::Duration::seconds(self.config.sync_interval_secs as i64);
        self.spawner
            .spawn_at_with_queue_id(JobId::new(), next_config, schedule_at, LIBRARY_LOCK_QUEUE)
            .await?;

        Ok(JobCompletion::Complete)
    }
}

impl SyncWorkflowsFromLibraryRunner {
    async fn sync_once(
        &self,
        current_job: &CurrentJob,
    ) -> Result<Option<String>, Box<dyn std::error::Error>> {
        let changes = self
            .library
            .find_new_workflows(self.config.last_sync_commit.as_deref())
            .await?;

        if changes.files.is_empty() {
            let commit = if changes.head_commit.is_empty() {
                None
            } else {
                Some(changes.head_commit)
            };
            return Ok(commit);
        }

        tracing::info!(
            count = changes.files.len(),
            head = %changes.head_commit,
            "processing changed workflow files from library"
        );

        // Workflows are always workspace-scoped (NOT NULL FK on the
        // table). Files under `runtime/workflows/` (no workspace) are
        // skipped with a warning rather than promoted to a global
        // workflow.
        let mut ws_cache: std::collections::HashMap<
            String,
            Option<crate::primitives::WorkspaceId>,
        > = std::collections::HashMap::new();
        let mut resolved: Vec<(
            &crate::library::WorkflowFileChange,
            crate::primitives::WorkspaceId,
        )> = Vec::with_capacity(changes.files.len());
        for change in &changes.files {
            let ws_name = match Self::workspace_name(&change.file) {
                Some(name) => name,
                None => {
                    tracing::warn!(
                        "workflow file lacks workspace folder; skipping (workflows must live under runtime/workspaces/<name>/workflows/)"
                    );
                    continue;
                }
            };
            let ws_id = match ws_cache.get(ws_name) {
                Some(cached) => *cached,
                None => {
                    let id = self.resolve_workspace_id(ws_name).await;
                    ws_cache.insert(ws_name.to_string(), id);
                    id
                }
            };
            match ws_id {
                Some(id) => resolved.push((change, id)),
                None => tracing::warn!(
                    workspace_name = %ws_name,
                    "workspace not found for workflow file; skipping"
                ),
            }
        }

        let mut op = current_job.begin_op().await?;
        for (change, ws_id) in &resolved {
            let file_hash = change.file.file_hash();
            if let Err(e) = self
                .workflows
                .upsert_from_library_in_op(&mut op, &change.file, *ws_id, file_hash)
                .await
            {
                tracing::warn!(error = %e, "failed to upsert workflow from library, skipping");
            }
        }
        op.commit().await?;

        Ok(Some(changes.head_commit))
    }

    fn workspace_name(file: &RuntimeFile) -> Option<&str> {
        match file {
            RuntimeFile::Workflow { workspace_name, .. } => workspace_name.as_deref(),
            _ => None,
        }
    }

    async fn resolve_workspace_id(&self, ws_name: &str) -> Option<crate::primitives::WorkspaceId> {
        match self.workspaces.find_by_name(ws_name).await {
            Ok(Some(ws)) => Some(ws.id),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(
                    workspace_name = %ws_name,
                    error = %e,
                    "failed to look up workspace"
                );
                None
            }
        }
    }
}
