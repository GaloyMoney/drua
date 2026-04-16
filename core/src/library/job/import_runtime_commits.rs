use std::path::PathBuf;
use std::time::Duration;

use job::*;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

const POLL_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Serialize, Deserialize)]
pub struct ImportRuntimeCommitsConfig {}

pub struct ImportRuntimeCommitsJobInitializer {
    pool: PgPool,
    repo_path: PathBuf,
    repo_url: Option<String>,
}

impl ImportRuntimeCommitsJobInitializer {
    pub fn new(pool: &PgPool, config: &super::super::LibraryConfig) -> Self {
        Self {
            pool: pool.clone(),
            repo_path: config.repo_path(),
            repo_url: config.repo_url.clone(),
        }
    }

    pub fn cfg() -> ImportRuntimeCommitsConfig {
        ImportRuntimeCommitsConfig {}
    }
}

impl JobInitializer for ImportRuntimeCommitsJobInitializer {
    type Config = ImportRuntimeCommitsConfig;

    fn job_type(&self) -> JobType {
        JobType::new("library.import-runtime-commits")
    }

    fn init(
        &self,
        _job: &Job,
        _spawner: JobSpawner<Self::Config>,
    ) -> Result<Box<dyn JobRunner>, Box<dyn std::error::Error>> {
        Ok(Box::new(ImportRuntimeCommitsRunner {
            pool: self.pool.clone(),
            repo_path: self.repo_path.clone(),
            repo_url: self.repo_url.clone(),
        }))
    }
}

struct ImportRuntimeCommitsRunner {
    #[allow(dead_code)]
    pool: PgPool,
    #[allow(dead_code)]
    repo_path: PathBuf,
    #[allow(dead_code)]
    repo_url: Option<String>,
}

#[async_trait::async_trait]
impl JobRunner for ImportRuntimeCommitsRunner {
    async fn run(
        &self,
        mut current_job: CurrentJob,
    ) -> Result<JobCompletion, Box<dyn std::error::Error>> {
        loop {
            if current_job.is_shutdown_requested() {
                tracing::info!("import-runtime-commits: shutdown requested, exiting");
                break;
            }

            // TODO: ensure clone at self.repo_path (clone from repo_url or local path)
            // TODO: git fetch origin main, diff refs, walk new commits
            // TODO: bidirectional sync — pull from remote, push local changes back

            tokio::time::sleep(POLL_INTERVAL).await;
        }
        Ok(JobCompletion::Complete)
    }
}
