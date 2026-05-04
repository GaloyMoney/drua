use std::sync::Arc;

use job::{CurrentJob, Job, JobCompletion, JobId, JobInitializer, JobRunner, JobSpawner, JobType};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex, RwLock};

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

pub(crate) type ImporterRegistry = Arc<RwLock<Vec<Arc<dyn LibraryImporter>>>>;

pub(crate) struct LibrarySyncJobInitializer {
    rx: Arc<Mutex<mpsc::Receiver<CommitTick>>>,
    git: Arc<GitEngine>,
    search: SearchStore,
    importers: ImporterRegistry,
    embed_spawner: JobSpawner<LibraryEmbedConfig>,
}

impl LibrarySyncJobInitializer {
    pub fn new(
        rx: mpsc::Receiver<CommitTick>,
        git: Arc<GitEngine>,
        search: SearchStore,
        importers: ImporterRegistry,
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
            importers: Arc::clone(&self.importers),
            embed_spawner: self.embed_spawner.clone(),
        }))
    }
}

struct LibrarySyncRunner {
    rx: Arc<Mutex<mpsc::Receiver<CommitTick>>>,
    git: Arc<GitEngine>,
    search: SearchStore,
    importers: ImporterRegistry,
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
                    // RescheduleNow, not Complete: `spawn_unique` is a no-op
                    // while a row exists, so Complete would mark this
                    // long-lived consumer terminal forever and the next pod
                    // boot would silently never re-attach.
                    return Ok(JobCompletion::RescheduleNow);
                }
                msg = rx.recv() => {
                    match msg {
                        Some(tick) => {
                            if state.last_processed_head.as_deref() == Some(tick.head.as_str()) {
                                continue;
                            }
                            tracing::debug!(head = %tick.head, "library.sync: processing tick");
                            // Advance `last_processed_head` only if the tick was
                            // actually processed. If `process_tick` fails before
                            // computing deltas (e.g. `changes_since` errors), the
                            // next tick must diff forward from the same start
                            // point or we'd lose every change in this commit
                            // range.
                            match self
                                .process_tick(
                                    &current_job,
                                    state.last_processed_head.as_deref(),
                                    &tick.head,
                                )
                                .await
                            {
                                Ok(()) => {
                                    state.last_processed_head = Some(tick.head);
                                    current_job.update_execution_state(state.clone()).await?;
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        error = %e,
                                        head = %tick.head,
                                        "library.sync: tick processing failed; \
                                         leaving last_processed_head unchanged \
                                         so the next tick reprocesses this range"
                                    );
                                }
                            }
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
    /// Returns `Err` only when the diff itself failed and no deltas could
    /// be computed. Per-delta dispatch failures are logged and swallowed
    /// (importers are idempotent), so they don't block the head advance.
    async fn process_tick(
        &self,
        current_job: &CurrentJob,
        from: Option<&str>,
        to: &str,
    ) -> Result<(), crate::LibraryError> {
        let deltas = self.git.changes_since(from, to).await.map_err(|e| {
            tracing::warn!(error = %e, ?from, to, "changes_since failed");
            e
        })?;

        let importers = self.importers.read().await;
        for delta in deltas {
            let importer = match importers.iter().find(|i| i.matches(&delta.path)) {
                Some(i) => i,
                None => continue,
            };
            if let Err(e) = self.dispatch(current_job, importer.as_ref(), &delta).await {
                tracing::warn!(error = %e, path = %delta.path, "library.sync: dispatch failed");
            }
        }
        Ok(())
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
