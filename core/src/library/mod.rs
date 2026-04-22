mod error;
mod job;

use std::path::PathBuf;

pub use error::LibraryError;
use self::job::PushRuntimeCommitsJobInitializer;

#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct LibraryConfig {
    /// Directory for the bare git clone. Defaults to `$TMPDIR/galoy-agents-library`.
    #[serde(default)]
    pub data_dir: Option<String>,
    /// Git remote URL or local path to the library repo.
    #[serde(default)]
    pub repo_url: Option<String>,
}

impl LibraryConfig {
    pub fn repo_path(&self) -> PathBuf {
        let base = match &self.data_dir {
            Some(d) => PathBuf::from(d),
            None => std::env::temp_dir(),
        };
        base.join("galoy-agents-library")
    }
}

#[derive(Clone)]
pub struct Library {
    repo_path: PathBuf,
}

impl Library {
    pub fn new(config: &LibraryConfig, jobs: &mut ::job::Jobs) -> Self {
        let init = PushRuntimeCommitsJobInitializer::new(config);
        let spawner = jobs.add_initializer(init);
        tokio::spawn(async move {
            if let Err(e) = spawner
                .spawn_unique(
                    ::job::JobId::new(),
                    PushRuntimeCommitsJobInitializer::cfg(),
                )
                .await
            {
                tracing::error!(error = %e, "Failed to spawn push-runtime-commits job");
            }
        });

        Self {
            repo_path: config.repo_path(),
        }
    }

    /// Write a file into the runtime area of the library repo, then
    /// immediately `git add` + `git commit` it.
    ///
    /// The file is written to `{repo_path}/{relative_path}`, creating
    /// intermediate directories as needed. The commit targets exactly the
    /// written file. The push-runtime-commits job will push the commit to
    /// the remote on its next cycle.
    #[tracing::instrument(name = "library.write_runtime_file", skip(self, content))]
    pub async fn write_runtime_file(
        &self,
        relative_path: &str,
        content: &str,
        commit_message: &str,
    ) -> Result<(), LibraryError> {
        let full_path = self.repo_path.join(relative_path);
        if let Some(parent) = full_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| LibraryError::Io(e.to_string()))?;
        }
        tokio::fs::write(&full_path, content)
            .await
            .map_err(|e| LibraryError::Io(e.to_string()))?;
        tracing::info!(path = %full_path.display(), "wrote runtime file");

        if self.repo_path.join(".git").exists() {
            self.git_commit(relative_path, commit_message).await?;
        }

        Ok(())
    }

    async fn git_commit(
        &self,
        relative_path: &str,
        message: &str,
    ) -> Result<(), LibraryError> {
        let add = tokio::process::Command::new("git")
            .args(["add", "--", relative_path])
            .current_dir(&self.repo_path)
            .output()
            .await
            .map_err(|e| LibraryError::Git(format!("git add: {e}")))?;
        if !add.status.success() {
            return Err(LibraryError::Git(format!(
                "git add failed: {}",
                String::from_utf8_lossy(&add.stderr)
            )));
        }

        let commit = tokio::process::Command::new("git")
            .args([
                "-c", "user.name=drua",
                "-c", "user.email=drua@galoy.io",
                "commit", "-m", message,
                "--", relative_path,
            ])
            .current_dir(&self.repo_path)
            .output()
            .await
            .map_err(|e| LibraryError::Git(format!("git commit: {e}")))?;
        if !commit.status.success() {
            return Err(LibraryError::Git(format!(
                "git commit failed: {}",
                String::from_utf8_lossy(&commit.stderr)
            )));
        }

        tracing::info!(path = relative_path, "committed runtime file");
        Ok(())
    }
}
