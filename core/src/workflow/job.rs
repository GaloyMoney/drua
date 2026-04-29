use std::sync::Arc;

use job::*;
use serde::{Deserialize, Serialize};

use crate::agent::Agents;
use crate::primitives::WorkflowRunId;
use crate::sandbox::Sandboxes;
use crate::skill::Skills;

use super::executor::Executor;
use super::repo::WorkflowDefinitionRepo;
use super::run::WorkflowRunRepo;

pub(crate) const EXECUTE_RUN_JOB: &str = "workflow.execute-run";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteRunConfig {
    pub run_id: WorkflowRunId,
}

pub struct ExecuteRunJobInitializer {
    runs: WorkflowRunRepo,
    definitions: WorkflowDefinitionRepo,
    agents: Arc<Agents>,
    skills: Arc<Skills>,
    sandboxes: Arc<Sandboxes>,
}

impl ExecuteRunJobInitializer {
    pub fn new(
        runs: WorkflowRunRepo,
        definitions: WorkflowDefinitionRepo,
        agents: Arc<Agents>,
        skills: Arc<Skills>,
        sandboxes: Arc<Sandboxes>,
    ) -> Self {
        Self {
            runs,
            definitions,
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
        let executor = Executor::new(
            self.runs.clone(),
            self.definitions.clone(),
            Arc::clone(&self.agents),
            Arc::clone(&self.skills),
            Arc::clone(&self.sandboxes),
        );
        Ok(Box::new(ExecuteRunRunner { executor, config }))
    }
}

struct ExecuteRunRunner {
    executor: Executor,
    config: ExecuteRunConfig,
}

#[async_trait::async_trait]
impl JobRunner for ExecuteRunRunner {
    #[tracing::instrument(name = "core.workflow.execute_run.job", skip_all, fields(run_id = %self.config.run_id))]
    async fn run(
        &self,
        _current_job: CurrentJob,
    ) -> Result<JobCompletion, Box<dyn std::error::Error>> {
        self.executor.run(self.config.run_id).await?;
        Ok(JobCompletion::Complete)
    }
}

/// Generic-runner JobType name for workflow reverse-sync. The runner
/// itself lives in `library::SyncFromLibraryJobInitializer<WorkflowDefinition>`.
pub(crate) const SYNC_WORKFLOWS_FROM_LIBRARY_JOB: &str = "workflow.sync-from-library";
