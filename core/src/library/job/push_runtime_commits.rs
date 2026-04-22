use std::path::PathBuf;
use std::time::Duration;

use job::*;
use serde::{Deserialize, Serialize};

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
            if current_job.is_shutdown_requested() {
                tracing::info!("push-runtime-commits: shutdown requested");
                break;
            }

            if let Err(e) = self.try_push().await {
                tracing::warn!(error = %e, "push-runtime-commits: push failed, will retry");
            }

            tokio::time::sleep(POLL_INTERVAL).await;
        }
        Ok(JobCompletion::Complete)
    }
}

impl PushRuntimeCommitsRunner {
    async fn try_push(&self) -> Result<(), Box<dyn std::error::Error>> {
        if !self.repo_path.join(".git").exists() {
            return Ok(());
        }

        // Check if there are commits to push (local ahead of remote)
        let status = tokio::process::Command::new("git")
            .args(["status", "--porcelain", "-b"])
            .current_dir(&self.repo_path)
            .output()
            .await?;
        let stdout = String::from_utf8_lossy(&status.stdout);
        if !stdout.contains("ahead") {
            return Ok(());
        }

        let push = tokio::process::Command::new("git")
            .args(["push"])
            .current_dir(&self.repo_path)
            .output()
            .await?;
        if !push.status.success() {
            return Err(format!(
                "git push failed: {}",
                String::from_utf8_lossy(&push.stderr)
            )
            .into());
        }

        tracing::info!("pushed runtime commits");
        Ok(())
    }
}
