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
use crate::primitives::WorkflowRunId;
use crate::sandbox::Sandboxes;
use crate::skill::Skills;

use super::executor;
use super::run::WorkflowRunRepo;

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
