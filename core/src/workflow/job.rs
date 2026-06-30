use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use job::*;
use serde::{Deserialize, Serialize};

use crate::agent::Agents;
use crate::primitives::WorkflowRunId;
use crate::sandbox::Sandboxes;
use crate::skill::Skills;
use crate::toolset::ToolSets;

use super::error::WorkflowError;
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
    toolsets: Arc<ToolSets>,
}

impl ExecuteRunJobInitializer {
    pub fn new(
        runs: WorkflowRunRepo,
        definitions: WorkflowDefinitionRepo,
        agents: Arc<Agents>,
        skills: Arc<Skills>,
        sandboxes: Arc<Sandboxes>,
        toolsets: Arc<ToolSets>,
    ) -> Self {
        Self {
            runs,
            definitions,
            agents,
            skills,
            sandboxes,
            toolsets,
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
            Arc::clone(&self.toolsets),
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
        mut current_job: CurrentJob,
    ) -> Result<JobCompletion, Box<dyn std::error::Error>> {
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_for_watcher = Arc::clone(&cancel);
        let watcher = tokio::spawn(async move {
            if current_job.shutdown_requested().await {
                cancel_for_watcher.store(true, Ordering::Release);
            }
        });

        let result = self.executor.run(self.config.run_id, cancel).await;
        watcher.abort();

        match result {
            Ok(()) => Ok(JobCompletion::Complete),
            Err(WorkflowError::Cancelled) => Ok(JobCompletion::RescheduleNow),
            Err(WorkflowError::RunFind(ref e)) if e.was_not_found() => {
                tracing::warn!(
                    run_id = %self.config.run_id,
                    "run deleted; completing job without retry"
                );
                Ok(JobCompletion::Complete)
            }
            Err(WorkflowError::DefinitionFind(ref e)) if e.was_not_found() => {
                tracing::warn!(
                    run_id = %self.config.run_id,
                    "definition deleted; completing job without retry"
                );
                Ok(JobCompletion::Complete)
            }
            Err(e) => Err(Box::new(e)),
        }
    }
}
