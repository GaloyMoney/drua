use job::*;
use serde::{Deserialize, Serialize};

use super::file::RuntimeFile;
use super::upstream::Upstream;

/// Shared queue ID that serializes all git operations on the library repo.
/// Both forward-sync (WriteToRuntime) and reverse-sync (SyncSkillsFromLibrary)
/// jobs use this queue to prevent concurrent access to the working directory.
pub const LIBRARY_LOCK_QUEUE: &str = "library-lock";

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct WriteToRuntimeConfig {
    pub file: RuntimeFile,
}

pub(super) struct WriteToRuntimeJobInitializer {
    upstream: Upstream,
}

impl WriteToRuntimeJobInitializer {
    pub fn new(upstream: Upstream) -> Self {
        Self { upstream }
    }
}

impl JobInitializer for WriteToRuntimeJobInitializer {
    type Config = WriteToRuntimeConfig;

    fn job_type(&self) -> JobType {
        JobType::new(super::WRITE_TO_RUNTIME_JOB)
    }

    fn init(
        &self,
        job: &Job,
        _spawner: JobSpawner<Self::Config>,
    ) -> Result<Box<dyn JobRunner>, Box<dyn std::error::Error>> {
        let config: WriteToRuntimeConfig = job.config()?;
        Ok(Box::new(WriteToRuntimeRunner {
            upstream: self.upstream.clone(),
            file: config.file,
        }))
    }
}

struct WriteToRuntimeRunner {
    upstream: Upstream,
    file: RuntimeFile,
}

#[async_trait::async_trait]
impl JobRunner for WriteToRuntimeRunner {
    #[tracing::instrument(name = "library.write_to_runtime.run", skip_all, fields(path = %self.file.relative_path()))]
    async fn run(
        &self,
        _current_job: CurrentJob,
    ) -> Result<JobCompletion, Box<dyn std::error::Error>> {
        self.upstream.pull().await?;

        let new_hash = self.file.file_hash();
        if self
            .upstream
            .file_hash_on_disk(&self.file.relative_path())
            .await
            .as_ref()
            == Some(&new_hash)
        {
            tracing::debug!("file hash unchanged, skipping write");
            return Ok(JobCompletion::Complete);
        }

        let err_msg = match self.write_and_push().await {
            Ok(()) => return Ok(JobCompletion::Complete),
            Err(e) => e.to_string(),
        };

        tracing::warn!(error = %err_msg, "write failed, resetting working tree");
        if let Err(reset_err) = self.upstream.reset_dirty_state().await {
            tracing::error!(error = %reset_err, "reset after failed write also failed");
        }
        Err(err_msg.into())
    }
}

impl WriteToRuntimeRunner {
    async fn write_and_push(&self) -> Result<(), Box<dyn std::error::Error>> {
        let canonical_path = self.file.relative_path();
        self.upstream
            .write_file(&canonical_path, &self.file.content())
            .await?;

        // If the file was imported under a non-canonical name and the
        // original still exists on disk (i.e. first write after import),
        // remove it and commit both changes together.
        let original = self.file.original_path();
        let needs_rename = original.is_some_and(|p| p != canonical_path);
        if needs_rename {
            let old_path = original.unwrap();
            if self.upstream.file_exists(old_path).await {
                self.upstream.remove_file(old_path).await?;
                self.upstream
                    .commit_paths(&[&canonical_path, old_path], &self.file.commit_message())
                    .await?;
            } else {
                // Original already gone (previous attempt or never cloned) —
                // just commit the canonical file.
                self.upstream
                    .add_and_commit(&canonical_path, &self.file.commit_message())
                    .await?;
            }
        } else {
            self.upstream
                .add_and_commit(&canonical_path, &self.file.commit_message())
                .await?;
        }

        self.upstream.push().await?;
        Ok(())
    }
}
