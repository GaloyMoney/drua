use std::sync::Arc;

use job::{CurrentJob, Job, JobCompletion, JobInitializer, JobRunner, JobSpawner, JobType};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex};

pub(crate) const LIBRARY_SYNC_JOB: &str = "library.sync";

#[derive(Debug, Clone)]
pub(crate) struct CommitTick {
    pub head: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct LibrarySyncConfig {}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct LibrarySyncState {
    last_processed_head: Option<String>,
}

pub(crate) struct LibrarySyncJobInitializer {
    rx: Arc<Mutex<mpsc::Receiver<CommitTick>>>,
}

impl LibrarySyncJobInitializer {
    pub fn new(rx: mpsc::Receiver<CommitTick>) -> Self {
        Self {
            rx: Arc::new(Mutex::new(rx)),
        }
    }
}

impl JobInitializer for LibrarySyncJobInitializer {
    type Config = LibrarySyncConfig;

    fn job_type(&self) -> JobType {
        JobType::new(LIBRARY_SYNC_JOB)
    }

    fn init(
        &self,
        _job: &Job,
        _spawner: JobSpawner<Self::Config>,
    ) -> Result<Box<dyn JobRunner>, Box<dyn std::error::Error>> {
        Ok(Box::new(LibrarySyncRunner {
            rx: Arc::clone(&self.rx),
        }))
    }
}

struct LibrarySyncRunner {
    rx: Arc<Mutex<mpsc::Receiver<CommitTick>>>,
}

#[async_trait::async_trait]
impl JobRunner for LibrarySyncRunner {
    #[tracing::instrument(name = "library.sync.run", skip_all)]
    async fn run(
        &self,
        mut current_job: CurrentJob,
    ) -> Result<JobCompletion, Box<dyn std::error::Error>> {
        let mut rx = self.rx.lock().await;
        let mut state: LibrarySyncState = current_job.execution_state()?.unwrap_or_default();
        loop {
            tokio::select! {
                _ = current_job.shutdown_requested() => {
                    tracing::debug!("library.sync: shutdown requested");
                    return Ok(JobCompletion::Complete);
                }
                msg = rx.recv() => {
                    match msg {
                        Some(tick) => {
                            if state.last_processed_head.as_deref() == Some(tick.head.as_str()) {
                                continue;
                            }
                            tracing::debug!(head = %tick.head, "library.sync: processing tick");
                            state.last_processed_head = Some(tick.head.clone());
                            current_job.update_execution_state(state.clone()).await?;
                        }
                        None => {
                            tracing::debug!("library.sync: tick channel closed, rescheduling");
                            return Ok(JobCompletion::RescheduleNow);
                        }
                    }
                }
            }
        }
    }
}
