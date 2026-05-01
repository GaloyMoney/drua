mod config;
mod error;
mod git;
mod job;
pub mod primitives;
mod search;
pub mod space;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

pub use config::LibraryConfig;
pub use error::LibraryError;
pub use github_app::GitHubAppTokenProvider;
pub use search::{SearchStore, SearchableFields};
pub use space::{NewSpace, Space, SpaceError, SpaceEvent};

use self::git::GitEngine;
use self::job::{CommitTick, LibrarySyncConfig, LibrarySyncJobInitializer};

#[allow(dead_code)]
pub struct Library {
    config: LibraryConfig,
    pool: sqlx::PgPool,
    embedder: Arc<code_assistant_core::embedder::Embedder>,
    github_app: Option<Arc<GitHubAppTokenProvider>>,
    git: Arc<GitEngine>,
    search: SearchStore,
    _fetcher: tokio::task::JoinHandle<()>,
}

impl Library {
    pub async fn init(
        pool: &sqlx::PgPool,
        config: &LibraryConfig,
        embedder: Arc<code_assistant_core::embedder::Embedder>,
        jobs: &mut ::job::Jobs,
        github_app: Option<Arc<GitHubAppTokenProvider>>,
    ) -> Result<Self, LibraryError> {
        let repo_path = PathBuf::from(&config.data_dir);
        let git = Arc::new(GitEngine::init(&config.repo_url, repo_path, github_app.clone()).await?);

        let (tick_tx, tick_rx) = mpsc::channel::<CommitTick>(64);
        let fetcher = Self::spawn_fetcher(
            Arc::clone(&git),
            tick_tx,
            Duration::from_millis(config.fetch_interval_ms),
        );

        // @@ pass a collection of dyn LibraryImporter
        let spawner = jobs.add_initializer(LibrarySyncJobInitializer::new(tick_rx));
        spawner
            .spawn_unique(::job::JobId::new(), LibrarySyncConfig::default())
            .await?;

        Ok(Self {
            config: LibraryConfig {
                data_dir: config.data_dir.clone(),
                repo_url: config.repo_url.clone(),
                fetch_interval_ms: config.fetch_interval_ms,
            },
            pool: pool.clone(),
            embedder,
            github_app,
            git,
            search: SearchStore::new(pool),
            _fetcher: fetcher,
        })
    }

    fn spawn_fetcher(
        git: Arc<GitEngine>,
        tick_tx: mpsc::Sender<CommitTick>,
        interval: Duration,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut last_head: Option<String> = None;
            loop {
                ticker.tick().await;
                match git.fetch_and_head().await {
                    Ok(Some(head)) => {
                        if last_head.as_deref() == Some(head.as_str()) {
                            continue;
                        }
                        last_head = Some(head.clone());
                        if tick_tx.send(CommitTick { head }).await.is_err() {
                            return;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "library fetcher: fetch failed");
                    }
                }
            }
        })
    }
}
