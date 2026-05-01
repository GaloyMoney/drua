use std::sync::Arc;

use job::{CurrentJob, Job, JobCompletion, JobId, JobInitializer, JobRunner, JobSpawner, JobType};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex};

use super::LibraryEmbedConfig;
use crate::git::{CommitDelta, DeltaKind, GitEngine};
use crate::importer::LibraryImporter;
use crate::search::SearchStore;

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
    git: Arc<GitEngine>,
    search: SearchStore,
    importers: Vec<Arc<dyn LibraryImporter>>,
    embed_spawner: JobSpawner<LibraryEmbedConfig>,
}

impl LibrarySyncJobInitializer {
    pub fn new(
        rx: mpsc::Receiver<CommitTick>,
        git: Arc<GitEngine>,
        search: SearchStore,
        importers: Vec<Arc<dyn LibraryImporter>>,
        embed_spawner: JobSpawner<LibraryEmbedConfig>,
    ) -> Self {
        Self {
            rx: Arc::new(Mutex::new(rx)),
            git,
            search,
            importers,
            embed_spawner,
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
            git: Arc::clone(&self.git),
            search: self.search.clone(),
            importers: self.importers.clone(),
            embed_spawner: self.embed_spawner.clone(),
        }))
    }
}

struct LibrarySyncRunner {
    rx: Arc<Mutex<mpsc::Receiver<CommitTick>>>,
    git: Arc<GitEngine>,
    search: SearchStore,
    importers: Vec<Arc<dyn LibraryImporter>>,
    embed_spawner: JobSpawner<LibraryEmbedConfig>,
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
                            self.process_tick(
                                &current_job,
                                state.last_processed_head.as_deref(),
                                &tick.head,
                            )
                            .await;
                            state.last_processed_head = Some(tick.head);
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

impl LibrarySyncRunner {
    async fn process_tick(&self, current_job: &CurrentJob, from: Option<&str>, to: &str) {
        let deltas = match self.git.changes_since(from, to).await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = %e, ?from, to, "changes_since failed");
                return;
            }
        };

        for delta in deltas {
            let importer = match self.importers.iter().find(|i| i.matches(&delta.path)) {
                Some(i) => i,
                None => continue,
            };
            if let Err(e) = self.dispatch(current_job, importer.as_ref(), &delta).await {
                tracing::warn!(error = %e, path = %delta.path, "library.sync: dispatch failed");
            }
        }
    }

    async fn dispatch(
        &self,
        current_job: &CurrentJob,
        importer: &dyn LibraryImporter,
        delta: &CommitDelta,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut op = current_job.begin_op().await?;
        match delta.kind {
            DeltaKind::Deleted => {
                if let Some(doc_id) = importer.delete_in_op(&mut op, &delta.path).await? {
                    self.search
                        .delete_in_op(&mut op, doc_id, importer.doc_type())
                        .await?;
                }
            }
            DeltaKind::Added | DeltaKind::Modified => {
                let fields = importer
                    .upsert_in_op(
                        &mut op,
                        None,
                        delta.file_hash.clone(),
                        &delta.path,
                        &delta.content,
                    )
                    .await?;
                if let Some(fields) = fields {
                    let doc_id = fields.doc_id;
                    let doc_type = fields.doc_type.clone();
                    self.search.upsert_in_op(&mut op, &fields).await?;
                    // Embedding runs concurrently — no queue id.
                    self.embed_spawner
                        .spawn_in_op(
                            &mut op,
                            JobId::new(),
                            LibraryEmbedConfig { doc_id, doc_type },
                        )
                        .await?;
                }
            }
        }
        op.commit().await?;
        Ok(())
    }
}
