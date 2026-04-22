mod error;
mod file;
mod job;
mod upstream;

use self::job::PushRuntimeCommitsJobInitializer;
use self::upstream::Upstream;
pub use error::LibraryError;
pub use file::{GitFileHash, RuntimeFile};

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
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
    pub fn repo_path(&self) -> std::path::PathBuf {
        match &self.data_dir {
            Some(d) => std::path::PathBuf::from(d),
            None => std::path::PathBuf::from(".library"),
        }
    }
}

#[derive(Clone)]
pub struct Library {
    upstream: Upstream,
}

impl Library {
    pub async fn init(
        config: &LibraryConfig,
        jobs: &mut ::job::Jobs,
    ) -> Result<Self, LibraryError> {
        let upstream = Upstream::init(config.repo_url.as_deref(), config.repo_path()).await?;

        let init = PushRuntimeCommitsJobInitializer::new(upstream.clone());
        let spawner = jobs.add_initializer(init);
        if let Err(e) = spawner
            .spawn_unique(::job::JobId::new(), PushRuntimeCommitsJobInitializer::cfg())
            .await
        {
            tracing::error!(error = %e, "Failed to spawn push-runtime-commits job");
        }

        Ok(Self { upstream })
    }

    #[tracing::instrument(name = "library.write", skip(self, file))]
    pub async fn write(&self, file: RuntimeFile) -> Result<(), LibraryError> {
        let relative_path = file.relative_path();
        let content = file.content();
        let commit_message = file.commit_message();

        let full_path = self.upstream.repo_path().join(&relative_path);
        if let Some(parent) = full_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| LibraryError::Io(e.to_string()))?;
        }
        tokio::fs::write(&full_path, content)
            .await
            .map_err(|e| LibraryError::Io(e.to_string()))?;
        tracing::info!(path = %full_path.display(), "wrote runtime file");

        self.upstream
            .add_and_commit(&relative_path, &commit_message)
            .await?;

        Ok(())
    }
}
