use std::time::Duration;

use job::*;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

const POLL_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Serialize, Deserialize)]
pub struct ImportLibCommitsConfig {}

pub struct ImportLibCommitsJobInitializer {
    pool: PgPool,
}

impl ImportLibCommitsJobInitializer {
    pub fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }

    pub fn cfg() -> ImportLibCommitsConfig {
        ImportLibCommitsConfig {}
    }
}

impl JobInitializer for ImportLibCommitsJobInitializer {
    type Config = ImportLibCommitsConfig;

    fn job_type(&self) -> JobType {
        JobType::new("library.import-lib-commits")
    }

    fn init(
        &self,
        _job: &Job,
        _spawner: JobSpawner<Self::Config>,
    ) -> Result<Box<dyn JobRunner>, Box<dyn std::error::Error>> {
        Ok(Box::new(ImportLibCommitsRunner {
            pool: self.pool.clone(),
        }))
    }
}

struct ImportLibCommitsRunner {
    #[allow(dead_code)]
    pool: PgPool,
}

#[async_trait::async_trait]
impl JobRunner for ImportLibCommitsRunner {
    async fn run(
        &self,
        mut current_job: CurrentJob,
    ) -> Result<JobCompletion, Box<dyn std::error::Error>> {
        loop {
            if current_job.is_shutdown_requested() {
                tracing::info!("import-lib-commits: shutdown requested, exiting");
                break;
            }

            // TODO: git fetch, diff refs, walk new commits, store in PG

            tokio::time::sleep(POLL_INTERVAL).await;
        }
        Ok(JobCompletion::Complete)
    }
}
