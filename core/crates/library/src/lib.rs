pub mod attribution;
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

pub use attribution::{CommitAttribution, CommitSubjectKind};
pub use config::LibraryConfig;
pub use error::LibraryError;
pub use github_app::GitHubAppTokenProvider;
pub use importer::{DocType, GitFileHash, LibraryImporter, UpsertError};
pub use job::{LivenessRef, WriteOp};
pub use primitives::SpaceId;
pub use search::{SearchHit, SearchStore, SearchableFields};
pub use space::{NewSpace, Space, SpaceError, SpaceEvent, Spaces, SPACE_DOC_TYPE};
pub use synced::LibrarySynced;

use self::git::GitEngine;
pub use self::git::{BatchRmwFn, BlobEntries, DirEntry, PathDates, PathDatesMap};
use self::job::{
    CommitTick, ImporterRegistry, LibraryEmbedConfig, LibraryEmbedJobInitializer,
    LibrarySyncConfig, LibrarySyncJobInitializer, LibraryWriteConfig, LibraryWriteJobInitializer,
};
use self::synced::{HookEntry, LibrarySyncHook};

#[allow(dead_code)]
#[derive(Clone)]
pub struct Library {
    config: LibraryConfig,
    pool: sqlx::PgPool,
    embedder: Arc<code_assistant_core::embedder::Embedder>,
    github_app: Option<Arc<GitHubAppTokenProvider>>,
    git: Arc<GitEngine>,
    search: SearchStore,
    spaces: Spaces,
    importers: ImporterRegistry,
    write_spawner: ::job::JobSpawner<LibraryWriteConfig>,
    embed_spawner: ::job::JobSpawner<LibraryEmbedConfig>,
    /// Fetcher task handle is wrapped in `Arc` so the `Library` itself
    /// can be `Clone` (consumers store it directly rather than via a
    /// further `Arc` wrapper).
    _fetcher: Arc<tokio::task::JoinHandle<()>>,
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
        let git = Arc::new(
            GitEngine::init(
                &config.repo_url,
                repo_path,
                github_app.clone(),
                pool.clone(),
            )
            .await?,
        );

        let search = SearchStore::new(pool, Arc::clone(&embedder));
        let spaces = Spaces::new(&git, pool);

        let embed_spawner = jobs.add_initializer(LibraryEmbedJobInitializer::new(
            search.clone(),
            Arc::clone(&embedder),
        ));

        // Importers registry is shared with the write job so it can delegate
        // liveness checks to the domain (per doc type). Built before the write
        // initializer; importers registered later via `register_importer` are
        // visible through the shared `Arc<RwLock<…>>`.
        let importers: ImporterRegistry =
            Arc::new(tokio::sync::RwLock::new(vec![
                Arc::new(spaces.clone()) as Arc<dyn LibraryImporter>
            ]));

        let write_spawner = jobs.add_initializer(LibraryWriteJobInitializer::new(
            Arc::clone(&git),
            Arc::clone(&importers),
        ));

        let (tick_tx, tick_rx) = mpsc::channel::<CommitTick>(64);
        let fetcher = Self::spawn_fetcher(
            Arc::clone(&git),
            tick_tx,
            Duration::from_millis(config.fetch_interval_ms),
            git.commit_notify(),
        );

        let spawner = jobs.add_resident_initializer(LibrarySyncJobInitializer::new(
            tick_rx,
            Arc::clone(&git),
            search.clone(),
            Arc::clone(&importers),
            embed_spawner.clone(),
        ));
        spawner.spawn(LibrarySyncConfig::default()).await?;

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
            importers,
            write_spawner,
            embed_spawner,
            _fetcher: Arc::new(fetcher),
        })
    }

    /// Register an importer post-init. Inserted at the FRONT of the
    /// registry so it takes precedence over importers registered earlier.
    /// First-match-wins dispatch: when a subtree-refining importer
    /// (e.g. `SkillsImporter` claiming `spaces/<slug>/skills/*.md`)
    /// would otherwise be shadowed by the built-in `Spaces` catch-all
    /// (matches all `spaces/<slug>/...`), the later registration wins.
    /// The next `CommitTick` and all subsequent ticks honour the new
    /// ordering.
    pub async fn register_importer(&self, importer: Arc<dyn LibraryImporter>) {
        self.importers.write().await.insert(0, importer);
    }

    fn spawn_fetcher(
        git: Arc<GitEngine>,
        tick_tx: mpsc::Sender<CommitTick>,
        interval: Duration,
        commit_notify: Arc<tokio::sync::Notify>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut last_head: Option<String> = None;
            loop {
                tokio::select! {
                    _ = ticker.tick() => {}
                    _ = commit_notify.notified() => {}
                }
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

    /// Bare-clone path. Callers should prefer `read_blob_at_head`,
    /// `list_dir_at_head`, and `walk_blobs_at_head` over poking at the
    /// filesystem directly — bare clones don't materialise files.
    pub fn repo_path(&self) -> &std::path::Path {
        self.git.repo_path()
    }

    /// Read a blob's bytes at HEAD. `Ok(None)` when the path doesn't
    /// exist (or HEAD is unborn).
    pub async fn read_blob_at_head(&self, path: &str) -> Result<Option<Vec<u8>>, LibraryError> {
        self.git.read_blob_at_head(path).await
    }

    /// List immediate children of a tree path at HEAD. `Ok(None)` when
    /// the directory doesn't exist. Empty `dir_path` lists the repo
    /// root.
    pub async fn list_dir_at_head(
        &self,
        dir_path: &str,
    ) -> Result<Option<Vec<DirEntry>>, LibraryError> {
        self.git.list_dir_at_head(dir_path).await
    }

    /// Recursively walk every blob under `dir_path` at HEAD. Returns
    /// `(path, content)` pairs (paths relative to repo root); a `dir_path`
    /// naming a blob yields just that blob. `Ok(None)` when the path
    /// doesn't exist.
    pub async fn walk_blobs_at_head(
        &self,
        dir_path: &str,
    ) -> Result<Option<BlobEntries>, LibraryError> {
        self.git.walk_blobs_at_head(dir_path).await
    }

    /// Dates for every blob under `prefix` (repo-relative, trailing
    /// slash optional) at HEAD, keyed relative to `prefix`. `Ok(None)`
    /// when HEAD is unborn. See [`GitEngine::path_dates_at_head`] for
    /// the caching and rename-tracking contract.
    pub async fn path_dates_at_head(
        &self,
        prefix: &str,
    ) -> Result<Option<Arc<PathDatesMap>>, LibraryError> {
        self.git.path_dates_at_head(prefix).await
    }

    /// Read a blob's bytes at an arbitrary commit. `Ok(None)` when the
    /// path doesn't exist at that commit. The ref-aware counterpart of
    /// [`Self::read_blob_at_head`] — used once a `space:` call resolves
    /// to a changeset target.
    pub async fn read_blob_at(
        &self,
        commit_oid: &str,
        path: &str,
    ) -> Result<Option<Vec<u8>>, LibraryError> {
        self.git.read_blob_at(commit_oid, path).await
    }

    /// List immediate children of a tree path at an arbitrary commit.
    /// The ref-aware counterpart of [`Self::list_dir_at_head`].
    pub async fn list_dir_at(
        &self,
        commit_oid: &str,
        dir_path: &str,
    ) -> Result<Option<Vec<DirEntry>>, LibraryError> {
        self.git.list_dir_at(commit_oid, dir_path).await
    }

    /// Recursively walk every blob under `dir_path` at an arbitrary
    /// commit. The ref-aware counterpart of [`Self::walk_blobs_at_head`].
    pub async fn walk_blobs_at(
        &self,
        commit_oid: &str,
        dir_path: &str,
    ) -> Result<Option<BlobEntries>, LibraryError> {
        self.git.walk_blobs_at(commit_oid, dir_path).await
    }

    /// Current target of `refname` (e.g. `refs/heads/drua/<id>`).
    /// `Ok(None)` when the ref doesn't exist locally.
    pub async fn resolve_ref(&self, refname: &str) -> Result<Option<String>, LibraryError> {
        self.git.resolve_ref(refname).await
    }

    /// Points `refname` at `oid` locally (no push). See
    /// [`GitEngine::create_ref`].
    pub async fn create_ref(&self, refname: &str, oid: &str) -> Result<(), LibraryError> {
        self.git.create_ref(refname, oid).await
    }

    /// Deletes `refname` locally, and on origin too when `push` is set.
    pub async fn delete_ref(&self, refname: &str, push: bool) -> Result<(), LibraryError> {
        self.git.delete_ref(refname, push).await
    }

    /// Best common ancestor of `a` and `b`. `Ok(None)` when they share
    /// no history.
    pub async fn merge_base(&self, a: &str, b: &str) -> Result<Option<String>, LibraryError> {
        self.git.merge_base(a, b).await
    }

    /// Tree-level 3-way merge of `ours` onto `theirs` from `base`. See
    /// [`GitEngine::merge_trees`].
    pub async fn merge_trees(
        &self,
        base: &str,
        ours: &str,
        theirs: &str,
    ) -> Result<Result<String, Vec<String>>, LibraryError> {
        self.git.merge_trees(base, ours, theirs).await
    }

    /// Merges `changeset_tip` into `main` as a 2-parent commit. See
    /// [`GitEngine::merge_into_main`].
    pub async fn merge_into_main(
        &self,
        changeset_tip: &str,
        message: String,
        attribution: CommitAttribution,
    ) -> Result<String, LibraryError> {
        self.git
            .merge_into_main(changeset_tip, message, attribution)
            .await
    }

    /// Force-rewrites `refname` to a single squash commit onto `onto`.
    /// See [`GitEngine::rebase_ref`].
    pub async fn rebase_ref(
        &self,
        refname: &str,
        onto: &str,
        message: String,
        attribution: CommitAttribution,
    ) -> Result<Result<(String, String), Vec<String>>, LibraryError> {
        self.git
            .rebase_ref(refname, onto, message, attribution)
            .await
    }

    /// Ref-aware counterpart of a blind-overwrite write: `target_ref`
    /// selects the branch to commit and push to (`None` =
    /// `refs/heads/main`). Returns the resulting commit oid, or `None`
    /// if the write was a no-op.
    pub async fn write_file_at(
        &self,
        target_ref: Option<String>,
        path: String,
        content: Vec<u8>,
        commit_message: String,
        attribution: CommitAttribution,
    ) -> Result<Option<String>, LibraryError> {
        self.git
            .write_file_at(target_ref, path, content, commit_message, attribution)
            .await
    }

    /// Ref-aware counterpart of a delete. See [`Self::write_file_at`]
    /// for the `target_ref` contract.
    pub async fn delete_file_at(
        &self,
        target_ref: Option<String>,
        path: String,
        commit_message: String,
        attribution: CommitAttribution,
    ) -> Result<Option<String>, LibraryError> {
        self.git
            .delete_file_at(target_ref, path, commit_message, attribution)
            .await
    }

    /// Ref-aware read-modify-write. See [`Self::write_file_at`] for the
    /// `target_ref` contract.
    pub async fn update_file_at(
        &self,
        target_ref: Option<String>,
        path: String,
        update: BatchRmwFn,
        commit_message: String,
        attribution: CommitAttribution,
    ) -> Result<Option<String>, LibraryError> {
        self.git
            .update_file_at(target_ref, path, update, commit_message, attribution)
            .await
    }

    /// Ref-aware rename. See [`Self::write_file_at`] for the
    /// `target_ref` contract.
    pub async fn move_file_at(
        &self,
        target_ref: Option<String>,
        from: String,
        to: String,
        commit_message: String,
        attribution: CommitAttribution,
    ) -> Result<Option<String>, LibraryError> {
        self.git
            .move_file_at(target_ref, from, to, commit_message, attribution)
            .await
    }

    /// Per-repo `post_persist_hook` body collapses to a one-liner over
    /// this: projects the entity into a write op and registers the
    /// commit hook when at least one persisted event was a content
    /// event.
    ///
    /// Reads the [`CommitAttribution`] from the request's `EventContext`
    /// (populated by `drua_core::audit::Audit::commit_attribution`);
    /// falls back to the library default when nothing was captured.
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
        // A forward-sync write only projects already-persisted DB state into
        // git, so mark it: reverse-sync drops it instead of re-ingesting (the
        // workflow-delete domino). Direct authoring writes (`spaces edit`) go
        // through their own path unmarked and still import.
        let mut attribution = CommitAttribution::from_event_context();
        attribution.mark_projection();
        self.enqueue_in_op(
            op,
            HookEntry {
                fields: Some(entity.searchable_fields()),
                deletes: entity.extra_search_deletes(),
                write_op: Some(entity.write_op()),
                attribution,
                liveness: entity.liveness_guard(),
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
        attribution: CommitAttribution,
    ) -> Result<(), LibraryError> {
        self.enqueue_in_op(
            op,
            HookEntry {
                fields: None,
                deletes: Vec::new(),
                write_op: Some(write_op),
                attribution,
                liveness: None,
            },
        )
        .await
    }

    /// Full-control enqueue used by external crates that compute the
    /// `(fields, deletes, write_op)` projection themselves rather than
    /// implementing [`LibrarySynced`].
    #[tracing::instrument(name = "library.enqueue_full_in_op", skip_all)]
    pub async fn enqueue_full_in_op(
        &self,
        op: &mut impl es_entity::AtomicOperation,
        fields: Option<SearchableFields>,
        deletes: Vec<(uuid::Uuid, DocType)>,
        write_op: Option<WriteOp>,
        attribution: CommitAttribution,
    ) -> Result<(), LibraryError> {
        if fields.is_none() && deletes.is_empty() && write_op.is_none() {
            return Ok(());
        }
        self.enqueue_in_op(
            op,
            HookEntry {
                fields,
                deletes,
                write_op,
                attribution,
                liveness: None,
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
        attribution: CommitAttribution,
    ) -> Result<(), LibraryError> {
        self.search.delete_for_scope_in_op(op, scope_id).await?;
        self.enqueue_write_in_op(
            op,
            WriteOp::DeleteDir {
                path: dir_path,
                message: commit_message,
            },
            attribution,
        )
        .await
    }

    async fn enqueue_in_op(
        &self,
        op: &mut impl es_entity::AtomicOperation,
        entry: HookEntry,
    ) -> Result<(), LibraryError> {
        use es_entity::operation::hooks::CommitHook as _;
        let hook = LibrarySyncHook::new(
            self.write_spawner.clone(),
            self.embed_spawner.clone(),
            self.search.clone(),
            entry,
        );
        if let Err(hook) = op.add_commit_hook(hook) {
            hook.force_execute_pre_commit(op)
                .await
                .map_err(LibraryError::Sqlx)?;
        }
        Ok(())
    }
}
