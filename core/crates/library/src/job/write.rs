use std::sync::Arc;

use job::{CurrentJob, Job, JobCompletion, JobInitializer, JobRunner, JobSpawner, JobType};
use serde::{Deserialize, Serialize};

use super::sync::ImporterRegistry;
use crate::attribution::CommitAttribution;
use crate::git::GitEngine;
use crate::importer::DocType;

pub(crate) const LIBRARY_WRITE_JOB: &str = "library.write";

/// Single unit of work the write job applies to the upstream repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WriteOp {
    WriteFile {
        path: String,
        content: Vec<u8>,
        message: String,
    },
    /// Write at `new_path` and remove `old_path` (used when an imported
    /// file is rewritten to canonical form).
    WriteFileWithRename {
        old_path: String,
        new_path: String,
        content: Vec<u8>,
        message: String,
    },
    DeleteFile {
        path: String,
        message: String,
    },
    DeleteDir {
        path: String,
        message: String,
    },
    /// Multiple changes committed in one push (e.g. project init's
    /// notes/skills/workflows `.gitkeep` markers).
    Batch {
        changes: Vec<(String, Option<Vec<u8>>)>,
        message: String,
    },
    Move {
        from: String,
        to: String,
        message: String,
    },
}

/// Identifies the DB row a forward-sync write reflects, so the write job can
/// re-check liveness at execution time. A persisted write job captures its
/// bytes at enqueue time and may replay much later (restart, retry); if the
/// source entity was soft-deleted in the meantime, applying the captured
/// write would resurrect an orphan file.
///
/// Opaque to this crate: just `(doc_type, id)`. The matching
/// [`crate::LibraryImporter::is_doc_live`] owns the actual liveness query, so
/// the generic library crate stays decoupled from concrete entity tables.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LivenessRef {
    pub doc_type: DocType,
    pub id: uuid::Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LibraryWriteConfig {
    pub op: WriteOp,
    /// Captured at enqueue time so the rendered author/committer/trailer
    /// block reflects the originating request even after the persisted
    /// job is replayed across a process restart.
    #[serde(default)]
    pub attribution: CommitAttribution,
    /// When set, the write is skipped if its source row is soft-deleted (or
    /// gone) by the time the job runs — guards against a stale forward sync
    /// resurrecting a deleted entity's file.
    #[serde(default)]
    pub liveness: Option<LivenessRef>,
}

pub(crate) struct LibraryWriteJobInitializer {
    git: Arc<GitEngine>,
    importers: ImporterRegistry,
}

impl LibraryWriteJobInitializer {
    pub fn new(git: Arc<GitEngine>, importers: ImporterRegistry) -> Self {
        Self { git, importers }
    }
}

impl JobInitializer for LibraryWriteJobInitializer {
    type Config = LibraryWriteConfig;

    fn job_type(&self) -> JobType {
        JobType::new(LIBRARY_WRITE_JOB)
    }

    fn init(
        &self,
        job: &Job,
        _spawner: JobSpawner<Self::Config>,
    ) -> Result<Box<dyn JobRunner>, Box<dyn std::error::Error>> {
        let config: LibraryWriteConfig = job.config()?;
        Ok(Box::new(LibraryWriteRunner {
            git: Arc::clone(&self.git),
            importers: Arc::clone(&self.importers),
            op: config.op,
            attribution: config.attribution,
            liveness: config.liveness,
        }))
    }
}

struct LibraryWriteRunner {
    git: Arc<GitEngine>,
    importers: ImporterRegistry,
    op: WriteOp,
    attribution: CommitAttribution,
    liveness: Option<LivenessRef>,
}

impl LibraryWriteRunner {
    /// `true` when the write's source entity is soft-deleted or gone, so the
    /// captured (possibly replayed) write should be skipped. The actual check
    /// is delegated to the registered [`crate::LibraryImporter`] for the doc
    /// type. On a missing importer or a lookup error, treats the source as
    /// live and proceeds — matching the no-guard default.
    async fn source_is_dead(&self, lr: &LivenessRef) -> bool {
        let importers = self.importers.read().await;
        let Some(importer) = importers.iter().find(|i| i.doc_type() == lr.doc_type) else {
            tracing::warn!(
                doc_type = %lr.doc_type,
                "library.write: no importer for liveness check; proceeding"
            );
            return false;
        };
        match importer.is_doc_live(lr.id).await {
            Ok(live) => !live,
            Err(e) => {
                tracing::warn!(error = %e, id = %lr.id, "library.write: liveness check failed; proceeding");
                false
            }
        }
    }
}

#[async_trait::async_trait]
impl JobRunner for LibraryWriteRunner {
    #[tracing::instrument(name = "library.write.run", skip_all)]
    async fn run(
        &self,
        _current_job: CurrentJob,
    ) -> Result<JobCompletion, Box<dyn std::error::Error>> {
        if let Some(lr) = &self.liveness {
            if self.source_is_dead(lr).await {
                tracing::info!(
                    doc_type = %lr.doc_type,
                    id = %lr.id,
                    "library.write: skipping stale write; source soft-deleted or gone"
                );
                return Ok(JobCompletion::Complete);
            }
        }
        let attribution = self.attribution.clone();
        match &self.op {
            WriteOp::WriteFile {
                path,
                content,
                message,
            } => {
                self.git
                    .write_file(path.clone(), content.clone(), message.clone(), attribution)
                    .await?;
            }
            WriteOp::WriteFileWithRename {
                old_path,
                new_path,
                content,
                message,
            } => {
                self.git
                    .write_with_rename(
                        old_path.clone(),
                        new_path.clone(),
                        content.clone(),
                        message.clone(),
                        attribution,
                    )
                    .await?;
            }
            WriteOp::DeleteFile { path, message } => {
                self.git
                    .delete_file(path.clone(), message.clone(), attribution)
                    .await?;
            }
            WriteOp::DeleteDir { path, message } => {
                self.git
                    .delete_dir(path.clone(), message.clone(), attribution)
                    .await?;
            }
            WriteOp::Batch { changes, message } => {
                self.git
                    .commit_changes(changes.clone(), message.clone(), attribution)
                    .await?;
            }
            WriteOp::Move { from, to, message } => {
                self.git
                    .move_file(from.clone(), to.clone(), message.clone(), attribution)
                    .await?;
            }
        }
        Ok(JobCompletion::Complete)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_predating_liveness_field_still_deserializes() {
        // Write jobs persisted before `liveness` existed must keep loading
        // (serde default → None), or a restart would fail to replay them.
        let json = r#"{"op":{"kind":"delete_file","path":"a.yml","message":"m"}}"#;
        let cfg: LibraryWriteConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.liveness.is_none());
    }

    #[test]
    fn config_with_liveness_round_trips() {
        let cfg = LibraryWriteConfig {
            op: WriteOp::WriteFile {
                path: "a.yml".into(),
                content: b"x".to_vec(),
                message: "m".into(),
            },
            attribution: CommitAttribution::default(),
            liveness: Some(LivenessRef {
                doc_type: DocType::new("workflow"),
                id: uuid::Uuid::nil(),
            }),
        };
        let back: LibraryWriteConfig =
            serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        let lr = back.liveness.expect("liveness present");
        assert_eq!(lr.doc_type, DocType::new("workflow"));
    }
}
