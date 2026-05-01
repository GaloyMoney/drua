mod config;
mod error;
mod git;
mod importer;
mod job;
pub mod primitives;
mod search;
pub mod space;
mod synced;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

pub use config::LibraryConfig;
pub use error::LibraryError;
pub use github_app::GitHubAppTokenProvider;
pub use importer::{DocType, GitFileHash, LibraryImporter, UpsertError};
pub use job::WriteOp;
pub use search::{SearchHit, SearchStore, SearchableFields};
pub use space::{NewSpace, Space, SpaceError, SpaceEvent, Spaces};
pub use synced::LibrarySynced;

use self::git::GitEngine;
use self::job::{
    CommitTick, LibraryEmbedJobInitializer, LibrarySyncConfig, LibrarySyncJobInitializer,
    LibraryWriteConfig, LibraryWriteJobInitializer,
};
use self::synced::{HookEntry, LibrarySyncHook};

#[allow(dead_code)]
pub struct Library {
    config: LibraryConfig,
    pool: sqlx::PgPool,
    embedder: Arc<code_assistant_core::embedder::Embedder>,
    github_app: Option<Arc<GitHubAppTokenProvider>>,
    git: Arc<GitEngine>,
    search: SearchStore,
    spaces: Spaces,
    write_spawner: ::job::JobSpawner<LibraryWriteConfig>,
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

        let search = SearchStore::new(pool, Arc::clone(&embedder));
        let spaces = Spaces::new(&git, pool);

        let embed_spawner = jobs.add_initializer(LibraryEmbedJobInitializer::new(
            search.clone(),
            Arc::clone(&embedder),
        ));

        let write_spawner = jobs.add_initializer(LibraryWriteJobInitializer::new(Arc::clone(&git)));

        let (tick_tx, tick_rx) = mpsc::channel::<CommitTick>(64);
        let fetcher = Self::spawn_fetcher(
            Arc::clone(&git),
            tick_tx,
            Duration::from_millis(config.fetch_interval_ms),
        );

        let importers: Vec<Arc<dyn LibraryImporter>> = vec![Arc::new(spaces.clone())];

        let spawner = jobs.add_initializer(LibrarySyncJobInitializer::new(
            tick_rx,
            Arc::clone(&git),
            search.clone(),
            importers,
            embed_spawner,
        ));
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
            search,
            spaces,
            write_spawner,
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

    pub fn spaces(&self) -> &Spaces {
        &self.spaces
    }

    pub fn search(&self) -> &SearchStore {
        &self.search
    }

    /// Per-repo `post_persist_hook` body collapses to a one-liner over
    /// this: projects the entity into a write op and registers the
    /// commit hook when at least one persisted event was a content
    /// event.
    #[tracing::instrument(name = "library.sync_entity_in_op", skip_all)]
    pub async fn sync_entity_in_op<E, OP>(
        &self,
        op: &mut OP,
        entity: &E,
        new_events: &mut es_entity::LastPersisted<'_, E::Event>,
    ) -> Result<(), LibraryError>
    where
        E: LibrarySynced,
        OP: es_entity::AtomicOperation,
    {
        if !new_events.any(|p| E::is_content_event(&p.event)) {
            return Ok(());
        }
        self.enqueue_in_op(
            op,
            HookEntry {
                fields: Some(entity.searchable_fields()),
                deletes: entity.extra_search_deletes(),
                write_op: Some(entity.write_op()),
            },
        )
        .await
    }

    /// Lower-level: enqueue an arbitrary write op (and optional search
    /// row) on the per-transaction batch. Use for non-entity-backed
    /// work like project scaffolding or directory cleanup.
    #[tracing::instrument(name = "library.enqueue_write_in_op", skip_all)]
    pub async fn enqueue_write_in_op(
        &self,
        op: &mut impl es_entity::AtomicOperation,
        write_op: WriteOp,
    ) -> Result<(), LibraryError> {
        self.enqueue_in_op(
            op,
            HookEntry {
                fields: None,
                deletes: Vec::new(),
                write_op: Some(write_op),
            },
        )
        .await
    }

    /// Wipes search-index rows whose `scope_id` matches `scope_id` and
    /// queues a directory delete on the upstream repo. Call inside the
    /// scope-owner's delete transaction for atomicity.
    #[tracing::instrument(name = "library.cleanup_for_scope_in_op", skip_all, fields(%scope_id, %dir_path))]
    pub async fn cleanup_for_scope_in_op(
        &self,
        op: &mut impl es_entity::AtomicOperation,
        scope_id: uuid::Uuid,
        dir_path: String,
        commit_message: String,
    ) -> Result<(), LibraryError> {
        self.search.delete_for_scope_in_op(op, scope_id).await?;
        self.enqueue_write_in_op(
            op,
            WriteOp::DeleteDir {
                path: dir_path,
                message: commit_message,
            },
        )
        .await
    }

    async fn enqueue_in_op(
        &self,
        op: &mut impl es_entity::AtomicOperation,
        entry: HookEntry,
    ) -> Result<(), LibraryError> {
        use es_entity::operation::hooks::CommitHook as _;
        let hook = LibrarySyncHook::new(self.write_spawner.clone(), self.search.clone(), entry);
        if let Err(hook) = op.add_commit_hook(hook) {
            let _ = hook
                .force_execute_pre_commit(op)
                .await
                .map_err(LibraryError::Sqlx)?;
        }
        Ok(())
    }
}
