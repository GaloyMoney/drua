mod job;

use sqlx::PgPool;

use self::job::ImportLibCommitsJobInitializer;

#[derive(Clone)]
pub struct Library {
    #[allow(dead_code)]
    pool: PgPool,
}

impl Library {
    pub fn new(pool: &PgPool, jobs: &mut ::job::Jobs) -> Self {
        let initializer = ImportLibCommitsJobInitializer::new(pool);
        let spawner = jobs.add_initializer(initializer);
        // Spawn as unique-per-type so only one instance ever runs.
        tokio::spawn(async move {
            if let Err(e) = spawner
                .spawn_unique(::job::JobId::new(), ImportLibCommitsJobInitializer::cfg())
                .await
            {
                tracing::error!(error = %e, "Failed to spawn import-lib-commits job");
            }
        });
        Self { pool: pool.clone() }
    }
}
