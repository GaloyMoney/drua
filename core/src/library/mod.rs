mod error;
mod job;
mod upstream;

use std::path::PathBuf;

use self::job::PushRuntimeCommitsJobInitializer;
pub use error::LibraryError;

#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct LibraryConfig {
    /// Local path to clone the library repo into.
    /// Defaults to `<repo-root>/.library/`.
    #[serde(default)]
    pub data_dir: Option<String>,
    /// Git remote URL to clone from.
    #[serde(default)]
    pub repo_url: Option<String>,
}

impl LibraryConfig {
    pub fn repo_path(&self) -> PathBuf {
        match &self.data_dir {
            Some(d) => PathBuf::from(d),
            None => PathBuf::from(".library"),
        }
    }
}

#[derive(Clone)]
pub struct Library {
    repo_path: PathBuf,
}

impl Library {
    pub async fn init(
        config: &LibraryConfig,
        jobs: &mut ::job::Jobs,
    ) -> Result<Self, LibraryError> {
        let repo_path = config.repo_path();

        if let Some(repo_url) = &config.repo_url {
            if !repo_path.join(".git").exists() {
                upstream::clone(repo_url, &repo_path).await?;
            }
        }

        let init = PushRuntimeCommitsJobInitializer::new(config);
        let spawner = jobs.add_initializer(init);
        tokio::spawn(async move {
            if let Err(e) = spawner
                .spawn_unique(::job::JobId::new(), PushRuntimeCommitsJobInitializer::cfg())
                .await
            {
                tracing::error!(error = %e, "Failed to spawn push-runtime-commits job");
            }
        });

        Ok(Self { repo_path })
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
            upstream::add_and_commit(&self.repo_path, relative_path, commit_message).await?;
        }

        Ok(())
    }
}
