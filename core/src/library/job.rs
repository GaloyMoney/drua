use job::*;
use serde::{Deserialize, Serialize};

use super::file::UpstreamOp;
use super::upstream::Upstream;

/// Serializes all git ops on the library repo (both forward-sync
/// WriteToRuntime and reverse-sync SyncSkillsFromLibrary use this queue).
pub const LIBRARY_LOCK_QUEUE: &str = "library-lock";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct WriteToRuntimeConfig {
    pub file: UpstreamOp,
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
    file: UpstreamOp,
}

#[async_trait::async_trait]
impl JobRunner for WriteToRuntimeRunner {
    #[tracing::instrument(name = "library.write_to_runtime.run", skip_all, fields(path = %self.file.relative_path()))]
    async fn run(
        &self,
        _current_job: CurrentJob,
    ) -> Result<JobCompletion, Box<dyn std::error::Error>> {
        self.upstream.pull().await?;

        // Workspace/space init/cleanup: always execute, no hash comparison.
        match &self.file {
            UpstreamOp::WorkspaceInit { workspace_name } => {
                return self.workspace_init(workspace_name).await;
            }
            UpstreamOp::WorkspaceCleanup { workspace_name } => {
                return self.workspace_cleanup(workspace_name).await;
            }
            UpstreamOp::SpaceInit { slug } => {
                return self.space_init(slug).await;
            }
            UpstreamOp::WriteFile(_) => {}
        }

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
    async fn workspace_init(
        &self,
        workspace_name: &str,
    ) -> Result<JobCompletion, Box<dyn std::error::Error>> {
        let subdirs = ["notes", "skills", "workflows"];
        let paths: Vec<String> = subdirs
            .iter()
            .map(|s| format!("runtime/workspaces/{workspace_name}/{s}/.gitkeep"))
            .collect();
        let message = format!("workspace: init {workspace_name}");
        self.scaffold_or_reset(&paths, &message, "workspace init")
            .await
    }

    async fn space_init(&self, slug: &str) -> Result<JobCompletion, Box<dyn std::error::Error>> {
        let paths = vec![format!("spaces/{slug}/.gitkeep")];
        let message = format!("space: init {slug}");
        self.scaffold_or_reset(&paths, &message, "space init").await
    }

    async fn scaffold_or_reset(
        &self,
        paths: &[String],
        commit_message: &str,
        op_label: &'static str,
    ) -> Result<JobCompletion, Box<dyn std::error::Error>> {
        let err_msg = match self.scaffold_paths(paths, commit_message).await {
            Ok(()) => {
                self.upstream.push().await?;
                return Ok(JobCompletion::Complete);
            }
            Err(e) => e.to_string(),
        };

        tracing::warn!(error = %err_msg, op = op_label, "scaffold failed, resetting working tree");
        if let Err(reset_err) = self.upstream.reset_dirty_state().await {
            tracing::error!(error = %reset_err, op = op_label, "reset after failed scaffold also failed");
        }
        Err(err_msg.into())
    }

    async fn scaffold_paths(
        &self,
        paths: &[String],
        commit_message: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        for path in paths {
            self.upstream.write_file(path, "").await?;
        }
        let path_refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        self.upstream
            .commit_paths(&path_refs, commit_message)
            .await?;
        Ok(())
    }

    async fn workspace_cleanup(
        &self,
        workspace_name: &str,
    ) -> Result<JobCompletion, Box<dyn std::error::Error>> {
        let dir_path = format!("runtime/workspaces/{workspace_name}");
        let message = format!("workspace: delete {workspace_name}");

        let err_msg = match self
            .upstream
            .remove_dir_and_commit(&dir_path, &message)
            .await
        {
            Ok(()) => {
                self.upstream.push().await?;
                return Ok(JobCompletion::Complete);
            }
            Err(e) => e.to_string(),
        };

        tracing::warn!(error = %err_msg, "workspace cleanup failed, resetting working tree");
        if let Err(reset_err) = self.upstream.reset_dirty_state().await {
            tracing::error!(error = %reset_err, "reset after failed cleanup also failed");
        }
        Err(err_msg.into())
    }

    async fn write_and_push(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let canonical_path = self.file.relative_path();
        self.upstream
            .write_file(&canonical_path, &self.file.content())
            .await?;

        // First write after non-canonical import: remove original and
        // commit both changes together.
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
                // Original already gone — just commit the canonical file.
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
