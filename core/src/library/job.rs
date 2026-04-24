use job::*;
use serde::{Deserialize, Serialize};

use super::file::RuntimeFile;
use super::upstream::Upstream;

pub(super) const WRITE_TO_RUNTIME_QUEUE: &str = "write-to-runtime";

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

        let canonical_path = self.file.relative_path();
        self.upstream
            .write_file(&canonical_path, &self.file.content())
            .await?;

        // If the file was imported with a non-canonical name, remove the original.
        let original = self.file.original_path();
        let needs_rename = original.is_some_and(|p| p != canonical_path);
        if let Some(old_path) = original.filter(|_| needs_rename) {
            self.upstream.remove_file(old_path).await?;
            self.upstream
                .commit_paths(&[&canonical_path, old_path], &self.file.commit_message())
                .await?;
        } else {
            self.upstream
                .add_and_commit(&canonical_path, &self.file.commit_message())
                .await?;
        }
        self.upstream.push().await?;

        Ok(JobCompletion::Complete)
    }
}
