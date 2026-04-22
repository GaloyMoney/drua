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

            if let Err(e) = self.sync_once().await {
                tracing::error!(error = %e, "push-runtime-commits cycle failed");
            }

            tokio::time::sleep(POLL_INTERVAL).await;
        }
        Ok(JobCompletion::Complete)
    }
}

impl PushRuntimeCommitsRunner {
    async fn sync_once(&self) -> Result<(), Box<dyn std::error::Error>> {
        if !self.repo_path.join(".git").exists() {
            tracing::debug!(path = %self.repo_path.display(), "not a git repo, skipping");
            return Ok(());
        }

        let status = tokio::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&self.repo_path)
            .status()
            .await?;
        if !status.success() {
            return Err("git add -A failed".into());
        }

        // Exit code 1 = staged changes exist; 0 = clean
        let diff_status = tokio::process::Command::new("git")
            .args(["diff", "--cached", "--quiet"])
            .current_dir(&self.repo_path)
            .status()
            .await?;
        if diff_status.success() {
            return Ok(());
        }

        let commit_status = tokio::process::Command::new("git")
            .args([
                "-c",
                "user.name=drua",
                "-c",
                "user.email=drua@galoy.io",
                "commit",
                "-m",
                "auto: sync runtime notes",
            ])
            .current_dir(&self.repo_path)
            .status()
            .await?;
        if !commit_status.success() {
            return Err("git commit failed".into());
        }
        tracing::info!("committed runtime changes");

        let push_status = tokio::process::Command::new("git")
            .args(["push"])
            .current_dir(&self.repo_path)
            .status()
            .await?;
        if !push_status.success() {
            tracing::warn!("git push failed, will retry next cycle");
        } else {
            tracing::info!("pushed runtime changes");
        }

        Ok(())
    }
}
