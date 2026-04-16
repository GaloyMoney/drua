mod job;

use std::path::PathBuf;

use sqlx::PgPool;

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

        Self { pool: pool.clone() }
    }
}
