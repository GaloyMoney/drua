use std::sync::Arc;

use job::{CurrentJob, Job, JobCompletion, JobInitializer, JobRunner, JobSpawner, JobType};
use serde::{Deserialize, Serialize};

use crate::git::GitEngine;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LibraryWriteConfig {
    pub op: WriteOp,
}

pub(crate) struct LibraryWriteJobInitializer {
    git: Arc<GitEngine>,
}

impl LibraryWriteJobInitializer {
    pub fn new(git: Arc<GitEngine>) -> Self {
        Self { git }
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
            op: config.op,
        }))
    }
}

struct LibraryWriteRunner {
    git: Arc<GitEngine>,
    op: WriteOp,
}

#[async_trait::async_trait]
impl JobRunner for LibraryWriteRunner {
    #[tracing::instrument(name = "library.write.run", skip_all)]
    async fn run(
        &self,
        _current_job: CurrentJob,
    ) -> Result<JobCompletion, Box<dyn std::error::Error>> {
        match &self.op {
            WriteOp::WriteFile {
                path,
                content,
                message,
            } => {
                self.git
                    .write_file(path.clone(), content.clone(), message.clone())
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
                    )
                    .await?;
            }
            WriteOp::DeleteFile { path, message } => {
                self.git.delete_file(path.clone(), message.clone()).await?;
            }
            WriteOp::DeleteDir { path, message } => {
                self.git.delete_dir(path.clone(), message.clone()).await?;
            }
            WriteOp::Batch { changes, message } => {
                self.git
                    .commit_changes(changes.clone(), message.clone())
                    .await?;
            }
            WriteOp::Move { from, to, message } => {
                self.git
                    .move_file(from.clone(), to.clone(), message.clone())
                    .await?;
            }
        }
        Ok(JobCompletion::Complete)
    }
}
