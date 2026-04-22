use std::time::Duration;

use job::*;
use serde::{Deserialize, Serialize};

use super::super::upstream::Upstream;

const POLL_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Serialize, Deserialize)]
pub struct PushRuntimeCommitsConfig {}

pub struct PushRuntimeCommitsJobInitializer {
    upstream: Upstream,
}

impl PushRuntimeCommitsJobInitializer {
    pub fn new(upstream: Upstream) -> Self {
        Self { upstream }
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
            upstream: self.upstream.clone(),
        }))
    }
}

struct PushRuntimeCommitsRunner {
    upstream: Upstream,
}

#[async_trait::async_trait]
impl JobRunner for PushRuntimeCommitsRunner {
    async fn run(
        &self,
        mut current_job: CurrentJob,
    ) -> Result<JobCompletion, Box<dyn std::error::Error>> {
        loop {
            if let Err(e) = self.upstream.push_if_ahead().await {
                tracing::warn!(error = %e, "push-runtime-commits: push failed, will retry");
            }

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
