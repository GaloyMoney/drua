mod error;

use std::path::PathBuf;

pub use error::LibraryError;

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
    pub fn new(config: &LibraryConfig) -> Self {
        Self {
            repo_path: config.repo_path(),
        }
    }

    /// Write a file into the runtime area of the library repo.
    ///
    /// The file is written to `{repo_path}/{relative_path}`, creating
    /// intermediate directories as needed.
    #[tracing::instrument(name = "library.write_runtime_file", skip(self, content))]
    pub async fn write_runtime_file(
        &self,
        relative_path: &str,
        content: &str,
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
        Ok(())
    }
}
