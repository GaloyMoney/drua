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

        // Project/space init/cleanup: always execute, no hash comparison.
        match &self.file {
            UpstreamOp::ProjectInit { project_name } => {
                return self.project_init(project_name).await;
            }
            UpstreamOp::ProjectCleanup { project_name } => {
                return self.project_cleanup(project_name).await;
            }
            UpstreamOp::SpaceInit { slug } => {
                return self.space_init(slug).await;
            }
            UpstreamOp::SpaceFileWrite { content, .. } => {
                return self
                    .space_file_op(self.write_space_file_op(content).await)
                    .await;
            }
            UpstreamOp::SpaceFileDelete { .. } => {
                return self.space_file_op(self.delete_space_file_op().await).await;
            }
            UpstreamOp::SpaceFileStrReplace {
                old_str, new_str, ..
            } => {
                return self
                    .space_file_op(self.str_replace_space_file_op(old_str, new_str).await)
                    .await;
            }
            UpstreamOp::SpaceFileInsert {
                line_number, text, ..
            } => {
                return self
                    .space_file_op(self.insert_space_file_op(*line_number, text).await)
                    .await;
            }
            UpstreamOp::SpaceFileMove {
                slug,
                from_relative_path,
                to_relative_path,
            } => {
                return self
                    .space_file_op(
                        self.move_space_file_op(slug, from_relative_path, to_relative_path)
                            .await,
                    )
                    .await;
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
    async fn project_init(
        &self,
        project_name: &str,
    ) -> Result<JobCompletion, Box<dyn std::error::Error>> {
        let subdirs = ["notes", "skills", "workflows"];
        let paths: Vec<String> = subdirs
            .iter()
            .map(|s| format!("runtime/projects/{project_name}/{s}/.gitkeep"))
            .collect();
        let message = format!("project: init {project_name}");
        self.scaffold_or_reset(&paths, &message, "project init")
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

    /// Common wrapper for the four space-file ops: each helper returns
    /// the staged commit-or-error result; this dispatches push or
    /// resets the working tree on failure.
    async fn space_file_op(
        &self,
        staged: Result<(), Box<dyn std::error::Error + Send + Sync>>,
    ) -> Result<JobCompletion, Box<dyn std::error::Error>> {
        match staged {
            Ok(()) => {
                self.upstream.push().await?;
                Ok(JobCompletion::Complete)
            }
            Err(e) => {
                let err_msg = e.to_string();
                tracing::warn!(error = %err_msg, op = %self.file.commit_message(), "space-file op failed, resetting working tree");
                if let Err(reset_err) = self.upstream.reset_dirty_state().await {
                    tracing::error!(error = %reset_err, "reset after failed space-file op also failed");
                }
                Err(err_msg.into())
            }
        }
    }

    /// Blind overwrite. Hash short-circuit: if the freshest disk
    /// content already matches, no commit is produced (and `push` in
    /// the wrapper is still safe — it's a no-op on a clean tree).
    async fn write_space_file_op(
        &self,
        content: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let canonical_path = self.file.relative_path();
        let new_hash = self.file.file_hash();
        if self
            .upstream
            .file_hash_on_disk(&canonical_path)
            .await
            .as_ref()
            == Some(&new_hash)
        {
            tracing::debug!(path = %canonical_path, "space file unchanged, skipping write");
            return Ok(());
        }
        self.upstream.write_file(&canonical_path, content).await?;
        self.upstream
            .add_and_commit(&canonical_path, &self.file.commit_message())
            .await?;
        Ok(())
    }

    async fn delete_space_file_op(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let canonical_path = self.file.relative_path();
        if !self.upstream.file_exists(&canonical_path).await {
            tracing::debug!(path = %canonical_path, "space file already absent, skipping delete");
            return Ok(());
        }
        self.upstream.remove_file(&canonical_path).await?;
        self.upstream
            .commit_paths(&[&canonical_path], &self.file.commit_message())
            .await?;
        Ok(())
    }

    /// Read–modify–write inside the queue lock. Verifies `old_str`
    /// occurs exactly once on disk; aborts with a clean error otherwise.
    async fn str_replace_space_file_op(
        &self,
        old_str: &str,
        new_str: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let canonical_path = self.file.relative_path();
        let current = self.upstream.read_file(&canonical_path).await?;
        let count = current.matches(old_str).count();
        if count == 0 {
            return Err(format!("str_replace: old_str not found in {canonical_path}").into());
        }
        if count > 1 {
            return Err(format!(
                "str_replace: old_str appears {count} times in {canonical_path}; must be unique"
            )
            .into());
        }
        let new_content = current.replacen(old_str, new_str, 1);
        self.upstream
            .write_file(&canonical_path, &new_content)
            .await?;
        self.upstream
            .add_and_commit(&canonical_path, &self.file.commit_message())
            .await?;
        Ok(())
    }

    /// Read–modify–write inside the queue lock. `line_number == 0`
    /// inserts at the top of the file; otherwise inserts after that
    /// 1-based line number. Out-of-range line numbers append at EOF.
    async fn insert_space_file_op(
        &self,
        line_number: usize,
        text: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let canonical_path = self.file.relative_path();
        let current = self.upstream.read_file(&canonical_path).await?;
        let mut lines: Vec<String> = current.lines().map(String::from).collect();
        let idx = line_number.min(lines.len());
        for (offset, t) in text.lines().enumerate() {
            lines.insert(idx + offset, t.to_string());
        }
        let mut new_content = lines.join("\n");
        if current.ends_with('\n') {
            new_content.push('\n');
        }
        self.upstream
            .write_file(&canonical_path, &new_content)
            .await?;
        self.upstream
            .add_and_commit(&canonical_path, &self.file.commit_message())
            .await?;
        Ok(())
    }

    /// Renames `spaces/{slug}/{from}` → `spaces/{slug}/{to}`. Verifies
    /// pre-conditions on the freshest disk state inside the queue:
    /// src must exist, dest must not. Cross-space moves never reach
    /// here — the call site rejects different slugs.
    async fn move_space_file_op(
        &self,
        slug: &str,
        from: &str,
        to: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let from_path = format!("spaces/{slug}/{from}");
        let to_path = format!("spaces/{slug}/{to}");
        if from == to {
            return Err(format!("move: src and dest are the same: {from_path}").into());
        }
        if !self.upstream.file_exists(&from_path).await {
            return Err(format!("move: src does not exist: {from_path}").into());
        }
        if self.upstream.file_exists(&to_path).await {
            return Err(format!("move: dest already exists: {to_path}").into());
        }
        self.upstream.rename_file(&from_path, &to_path).await?;
        self.upstream
            .commit_paths(&[&from_path, &to_path], &self.file.commit_message())
            .await?;
        Ok(())
    }

    async fn project_cleanup(
        &self,
        project_name: &str,
    ) -> Result<JobCompletion, Box<dyn std::error::Error>> {
        let dir_path = format!("runtime/projects/{project_name}");
        let message = format!("project: delete {project_name}");

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

        tracing::warn!(error = %err_msg, "project cleanup failed, resetting working tree");
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
