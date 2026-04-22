use std::path::PathBuf;
use std::time::Duration;

use job::*;
use serde::{Deserialize, Serialize};

use super::upstream;

const POLL_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Serialize, Deserialize)]
pub struct PushRuntimeCommitsConfig {}

pub struct PushRuntimeCommitsJobInitializer {
    repo_path: PathBuf,
}

impl PushRuntimeCommitsJobInitializer {
    pub fn new(config: &super::super::LibraryConfig) -> Self {
        Self {
            repo_path: config.repo_path(),
        }
    }

    pub fn cfg() -> PushRuntimeCommitsConfig {
        PushRuntimeCommitsConfig {}
    }
}

impl JobInitializer for PushRuntimeCommitsJobInitializer {
    type Config = PushRuntimeCommitsConfig;

    fn job_type(&self) -> JobType {
        JobType::new("library.push-runtime-commits")
    }

    fn init(
        &self,
        _job: &Job,
        _spawner: JobSpawner<Self::Config>,
    ) -> Result<Box<dyn JobRunner>, Box<dyn std::error::Error>> {
        Ok(Box::new(PushRuntimeCommitsRunner {
            repo_path: self.repo_path.clone(),
        }))
    }
}

struct PushRuntimeCommitsRunner {
    repo_path: PathBuf,
}

#[async_trait::async_trait]
impl JobRunner for PushRuntimeCommitsRunner {
    async fn run(
        &self,
        mut current_job: CurrentJob,
    ) -> Result<JobCompletion, Box<dyn std::error::Error>> {
        loop {
            if self.repo_path.join(".git").exists() {
                if let Err(e) = upstream::push_if_ahead(&self.repo_path).await {
                    tracing::warn!(error = %e, "push-runtime-commits: push failed, will retry");
                }
            }

            // Wait for next cycle or shutdown, whichever comes first.
            tokio::select! {
                biased;
                shutdown = current_job.shutdown_requested() => {
                    if shutdown {
                        tracing::info!("push-runtime-commits: shutdown requested");
                        break;
                    }
                }
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
            }
        }
        Ok(JobCompletion::Complete)
    }
}
