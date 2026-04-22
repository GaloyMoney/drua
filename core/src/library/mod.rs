mod error;
mod job;

use std::path::PathBuf;

use sqlx::PgPool;

pub use error::LibraryError;
use self::job::{ImportLibCommitsJobInitializer, ImportRuntimeCommitsJobInitializer};

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
    #[allow(dead_code)]
    pool: PgPool,
    repo_path: PathBuf,
}

impl Library {
    pub fn new(pool: &PgPool, config: &LibraryConfig, jobs: &mut ::job::Jobs) -> Self {
        let lib_init = ImportLibCommitsJobInitializer::new(pool, config);
        let lib_spawner = jobs.add_initializer(lib_init);
        tokio::spawn(async move {
            if let Err(e) = lib_spawner
                .spawn_unique(::job::JobId::new(), ImportLibCommitsJobInitializer::cfg())
                .await
            {
                tracing::error!(error = %e, "Failed to spawn import-lib-commits job");
            }
        });

        let runtime_init = ImportRuntimeCommitsJobInitializer::new(pool, config);
        let runtime_spawner = jobs.add_initializer(runtime_init);
        tokio::spawn(async move {
            if let Err(e) = runtime_spawner
                .spawn_unique(
                    ::job::JobId::new(),
                    ImportRuntimeCommitsJobInitializer::cfg(),
                )
                .await
            {
                tracing::error!(error = %e, "Failed to spawn import-runtime-commits job");
            }
        });

        Self {
            pool: pool.clone(),
            repo_path: config.repo_path(),
        }
    }

    /// Write a file into the runtime area of the library repo.
    ///
    /// The file is written to `{repo_path}/{relative_path}`, creating
    /// intermediate directories as needed. The import-runtime-commits job
    /// will pick up the change and push it to the remote.
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
