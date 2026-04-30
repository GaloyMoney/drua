mod error;
mod file;
mod inbox;
mod job;
mod search;
pub mod space;
mod synced;
mod upstream;

use std::sync::Arc;

use es_entity::operation::hooks::CommitHook as _;

use crate::github_app::GitHubAppTokenProvider;

use self::inbox::LibraryWriteHandler;
use self::job::{WriteToRuntimeConfig, WriteToRuntimeJobInitializer};
use self::search::SearchStore;
use self::space::repo::SpaceRepo;
use self::upstream::Upstream;
pub use error::LibraryError;
pub(crate) use file::name_from_filename;
pub use file::{
    parse_skill_markdown, render_note_markdown, render_skill_markdown, DocType, GitFileHash,
    SearchableFields, UpstreamOp,
};
pub use job::LIBRARY_LOCK_QUEUE;
pub use search::{GlobalSearchHit, LibraryFile, SearchResult};
pub use space::{NewSpace, Space, SpaceError, SpaceEvent};
pub(crate) use synced::slugify;
pub use synced::{
    Changes, LibraryImporter, LibrarySynced, ParsedFile, SyncFromLibraryConfig,
    SyncFromLibraryJobInitializer, SyncedFile, UpsertError,
};
pub use upstream::SpaceFileChange;

const LIBRARY_WRITE_JOB: &str = "library.write";
const WRITE_TO_RUNTIME_JOB: &str = "library.write-to-runtime";

const DEFAULT_SKILL_SYNC_INTERVAL_SECS: u64 = 20;

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct LibraryConfig {
    /// Defaults to `<repo-root>/.library/`.
    #[serde(default)]
    pub data_dir: Option<String>,
    #[serde(default)]
    pub repo_url: Option<String>,
    /// Default 20s.
    #[serde(default = "default_skill_sync_interval_secs")]
    pub skill_sync_interval_secs: u64,
}

impl Default for LibraryConfig {
    fn default() -> Self {
        Self {
            data_dir: None,
            repo_url: None,
            skill_sync_interval_secs: DEFAULT_SKILL_SYNC_INTERVAL_SECS,
        }
    }
}

fn default_skill_sync_interval_secs() -> u64 {
    DEFAULT_SKILL_SYNC_INTERVAL_SECS
}

impl LibraryConfig {
    pub fn repo_path(&self) -> std::path::PathBuf {
        match &self.data_dir {
            Some(d) => std::path::PathBuf::from(d).join("repo"),
            None => std::path::PathBuf::from(".library"),
        }
    }
}

#[derive(Clone)]
pub struct Library {
    search: SearchStore,
    inbox: obix::Inbox,
    embedder: Arc<code_assistant_core::embedder::Embedder>,
    upstream: Upstream,
    space_repo: SpaceRepo,
    repo_url: Option<String>,
    /// Direct handle to the `WriteToRuntime` spawner so `create_space`
    /// can fire-and-await its scaffolding commit with a known `JobId`
    /// (the inbox path uses random `JobId::new()` and we need to
    /// `await_completions` on that specific job).
    write_spawner: ::job::JobSpawner<WriteToRuntimeConfig>,
    /// Cloned out of `Jobs::init` so `Library` can `await_completions`
    /// without plumbing `Arc<Jobs>` everywhere.
    jobs: ::job::Jobs,
}

impl Library {
    pub async fn init(
        config: &LibraryConfig,
        pool: &sqlx::PgPool,
        embedder: Arc<code_assistant_core::embedder::Embedder>,
        jobs: &mut ::job::Jobs,
        github_app: Option<Arc<GitHubAppTokenProvider>>,
    ) -> Result<Self, LibraryError> {
        let upstream =
            Upstream::init(config.repo_url.as_deref(), config.repo_path(), github_app).await?;
        let search = SearchStore::new(pool);

        let write_init = WriteToRuntimeJobInitializer::new(upstream.clone());
        let write_spawner = jobs.add_initializer(write_init);

        let handler =
            LibraryWriteHandler::new(search.clone(), embedder.clone(), write_spawner.clone());
        let inbox_config = obix::InboxConfig::new(::job::JobType::new(LIBRARY_WRITE_JOB));
        let inbox = obix::Inbox::new(pool, jobs, inbox_config, handler);

        Ok(Self {
            search,
            inbox,
            embedder,
            upstream,
            space_repo: SpaceRepo::new(pool),
            repo_url: config.repo_url.clone(),
            write_spawner,
            jobs: jobs.clone(),
        })
    }

    /// Library repo URL (from `LibraryConfig.repo_url`). `None` when the
    /// deployment runs without a library remote — caller-side feature
    /// gates use this to short-circuit `library_space` flows.
    pub fn repo_url(&self) -> Option<&str> {
        self.repo_url.as_deref()
    }

    /// Per-repo `post_persist_hook` body collapses to a one-liner over this:
    /// projects the entity into a `SyncedFile` and registers the write hook
    /// when at least one persisted event was a content event.
    #[tracing::instrument(
        name = "library.sync_entity_in_op",
        skip_all,
        fields(doc_type = E::DOC_TYPE.as_str())
    )]
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
        let file = UpstreamOp::WriteFile(Box::new(entity.to_synced_file()));
        self.enqueue_write(op, file.clone()).await?;
        Ok(())
    }

    async fn enqueue_write<OP: es_entity::AtomicOperation>(
        &self,
        op: &mut OP,
        file: UpstreamOp,
    ) -> Result<(), sqlx::Error> {
        let hook = synced::LibrarySyncHook::new(self.inbox.clone(), self.search.clone(), file);
        if let Err(hook) = op.add_commit_hook(hook) {
            let _ = hook.force_execute_pre_commit(op).await?;
        }
        Ok(())
    }

    /// Queues a single `WorkspaceInit` op — `notes/`, `skills/`,
    /// `workflows/` `.gitkeep` markers all land in one commit + push.
    #[tracing::instrument(name = "library.sync_workspace_folder_in_op", skip_all)]
    pub async fn sync_workspace_folder_in_op(
        &self,
        op: &mut impl es_entity::AtomicOperation,
        workspace_name: &str,
    ) -> Result<(), LibraryError> {
        let file = UpstreamOp::WorkspaceInit {
            workspace_name: workspace_name.to_string(),
        };
        self.enqueue_write(op, file).await?;
        Ok(())
    }

    /// Persists a new `Space` and queues `spaces/<slug>/.gitkeep` for
    /// upstream commit in the same transaction. The creating subject's
    /// workspace is auto-seeded into `authorized_workspaces`; non-agent
    /// subjects (e.g. plain `User`) produce a space with an empty
    /// authorized list.
    #[tracing::instrument(name = "library.create_space", skip(self, sub))]
    pub async fn create_space(
        &self,
        sub: &crate::auth::AuthSubject,
        slug: impl Into<String> + std::fmt::Debug,
        description: Option<String>,
    ) -> Result<Space, LibraryError> {
        sub.can(
            crate::auth::AuthVerb::Create,
            crate::auth::AuthResource::Space(None),
        )?;
        crate::audit::Audit::record_action_if_unset("space.create");

        let mut op = self.space_repo.begin_op().await?;
        let (space, job_id) = self
            .create_space_in_op(&mut op, sub, slug, description)
            .await?;
        op.commit().await?;

        // Block until the upstream scaffolding commit lands. Without
        // this, a follow-up `sandbox.create(library_space, slug)` can
        // race the push and end up with a sparse-checkout against a
        // commit that doesn't yet contain `spaces/<slug>/`.
        const SPACE_INIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
        let outcomes = self
            .jobs
            .await_completions(&[job_id], Some(SPACE_INIT_TIMEOUT))
            .await?;
        if !outcomes.iter().all(|o| o.is_completed()) {
            return Err(LibraryError::SpaceInitFailed {
                slug: space.slug.clone(),
            });
        }

        Ok(space)
    }

    /// Composable variant — caller owns the `op`. Skips the auth check
    /// (caller is expected to have authorised the broader transaction).
    /// Returns both the persisted `Space` and the `JobId` of the
    /// `WriteToRuntime` job that will commit `spaces/<slug>/.gitkeep`
    /// upstream. After committing the outer op, callers that need
    /// upstream synchrony should
    /// `library.jobs().await_completions(&[job_id], …)`.
    #[tracing::instrument(name = "library.create_space_in_op", skip(self, op, sub))]
    pub async fn create_space_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        sub: &crate::auth::AuthSubject,
        slug: impl Into<String> + std::fmt::Debug,
        description: Option<String>,
    ) -> Result<(Space, ::job::JobId), LibraryError> {
        let initial_workspaces: Vec<crate::primitives::WorkspaceId> =
            sub.workspace_id().into_iter().collect();

        let mut builder = NewSpace::builder()
            .slug(slug.into())
            .authorized_workspaces(initial_workspaces);
        if let Some(desc) = description {
            builder = builder.description(desc);
        }
        let new_space = builder.build()?;

        let space = self.space_repo.create_in_op(op, new_space).await?;

        // Spawn the upstream `.gitkeep` commit with a known `JobId` so
        // the standalone `create_space` can await this exact job.
        // Persisted in the same `op`, so the spawn is durable: a crash
        // before the outer commit drops both the entity and the job;
        // a crash after picks the job up from the persisted queue.
        let job_id = ::job::JobId::new();
        let config = WriteToRuntimeConfig {
            file: UpstreamOp::SpaceInit {
                slug: space.slug.clone(),
            },
        };
        self.write_spawner
            .spawn_with_queue_id_in_op(op, job_id, config, LIBRARY_LOCK_QUEUE)
            .await?;

        tracing::info!(space.id = %space.id, space.slug = %space.slug, "space created");
        Ok((space, job_id))
    }

    /// Cloned `Jobs` handle. Composable callers of `create_space_in_op`
    /// use this to `await_completions` after their outer op commits.
    pub fn jobs(&self) -> &::job::Jobs {
        &self.jobs
    }

    /// Resolves a slug to a `Space` after enforcing two checks:
    /// 1. The subject can read `AuthResource::Space(Some(space.id))`
    ///    (workspace admins are blanket-allowed by the scope layer).
    /// 2. The subject's workspace is in `space.authorized_workspaces`.
    ///
    /// Used by sandbox-creation flows that need to honor space ACLs.
    #[tracing::instrument(name = "library.find_space_by_slug_authorized", skip(self, sub))]
    pub async fn find_space_by_slug_authorized(
        &self,
        sub: &crate::auth::AuthSubject,
        slug: &str,
    ) -> Result<Space, LibraryError> {
        let space = self
            .space_repo
            .maybe_find_by_slug(slug)
            .await?
            .ok_or_else(|| SpaceError::NotFound {
                slug: slug.to_string(),
            })?;

        sub.can(
            crate::auth::AuthVerb::Read,
            crate::auth::AuthResource::Space(Some(space.id)),
        )?;
        crate::audit::Audit::record_action_if_unset("space.find_by_slug");

        let workspace_id = sub
            .workspace_id()
            .ok_or(crate::auth::error::AuthorizationError::AuthenticationRequired)?;
        if !space.is_workspace_authorized(workspace_id) {
            return Err(SpaceError::WorkspaceNotAuthorized {
                slug: space.slug.clone(),
                workspace_id,
            }
            .into());
        }

        Ok(space)
    }

    /// Internal access to the search/upstream/embedder primitives so
    /// `space::file_sync` can drive its index job without leaking
    /// `Library`'s privates to the rest of the crate.
    pub(in crate::library) fn search_store(&self) -> &SearchStore {
        &self.search
    }

    pub(in crate::library) fn embedder(&self) -> &Arc<code_assistant_core::embedder::Embedder> {
        &self.embedder
    }

    pub(in crate::library) fn upstream(&self) -> &Upstream {
        &self.upstream
    }

    /// No-auth slug → `Space` lookup used by the file-sync job to
    /// resolve `spaces/<slug>/...` paths to a space id.
    pub(in crate::library) async fn find_space_by_slug(
        &self,
        slug: &str,
    ) -> Result<Option<Space>, LibraryError> {
        Ok(self.space_repo.maybe_find_by_slug(slug).await?)
    }

    /// Removes search data and queues a job to delete
    /// `runtime/workspaces/<name>/` from the library repo. Call inside the
    /// workspace delete transaction for atomicity.
    #[tracing::instrument(name = "library.cleanup_workspace_in_op", skip_all)]
    pub async fn cleanup_workspace_in_op(
        &self,
        op: &mut impl es_entity::AtomicOperation,
        workspace_id: uuid::Uuid,
        workspace_name: &str,
    ) -> Result<(), LibraryError> {
        self.search
            .delete_for_workspace_in_op(op, workspace_id)
            .await?;

        let file = UpstreamOp::WorkspaceCleanup {
            workspace_name: workspace_name.to_string(),
        };
        self.enqueue_write(op, file).await?;
        Ok(())
    }

    /// Generic reverse-sync. Returns parsed `ParsedFile`s for every changed
    /// file under `S::Entity::DOC_TYPE`'s subdir since `last_sync_commit`.
    /// On first run (`None`), returns all tracked files. Empty `files` when
    /// HEAD hasn't moved.
    #[tracing::instrument(name = "library.find_changes", skip(self))]
    pub async fn find_changes<S: LibraryImporter>(
        &self,
        last_sync_commit: Option<&str>,
    ) -> Result<Changes, LibraryError> {
        self.upstream.pull().await?;

        let head = self.upstream.head_commit_hash().await?;
        let head = match head {
            Some(h) => h,
            None => {
                tracing::debug!("no commits in library repo");
                return Ok(Changes {
                    head_commit: String::new(),
                    files: Vec::new(),
                });
            }
        };

        if last_sync_commit == Some(head.as_str()) {
            return Ok(Changes {
                head_commit: head,
                files: Vec::new(),
            });
        }

        let doc_type = <S::Entity as LibrarySynced>::DOC_TYPE;
        let changed = self
            .upstream
            .changed_files(last_sync_commit, doc_type.subdir(), doc_type.ext())
            .await?;

        let mut files = Vec::with_capacity(changed.len());
        for (path, content) in &changed {
            match S::parse(content, path) {
                Some(parsed) => files.push(parsed),
                None => {
                    tracing::warn!(
                        path = %path,
                        doc_type = doc_type.as_str(),
                        "failed to parse library file, skipping"
                    );
                }
            }
        }

        Ok(Changes {
            head_commit: head,
            files,
        })
    }

    #[tracing::instrument(name = "library.search", skip(self))]
    pub async fn search(
        &self,
        workspace_id: uuid::Uuid,
        query: &str,
        doc_type: Option<DocType>,
        limit: usize,
    ) -> Result<Vec<SearchResult>, LibraryError> {
        let query_embedding = match self.embedder.embed_query(query).await {
            Ok(emb) => Some(emb),
            Err(e) => {
                tracing::warn!(error = %e, "embedding query failed, falling back to FTS-only");
                None
            }
        };
        self.search
            .search(workspace_id, query, query_embedding, doc_type, limit)
            .await
    }

    /// Cross-workspace search. Open to any non-anonymous subject;
    /// library content is globally discoverable. Empty `workspace_ids`
    /// = no workspace filter (every workspace plus global content);
    /// otherwise hits are restricted to the supplied ids (plus the nil
    /// UUID for global, auto-appended).
    ///
    /// Workflows are hosted in the library repo but excluded from search:
    /// passing an empty `doc_types` defaults to `[Skill, Note]`, and any
    /// `Workflow` entry in `doc_types` is silently dropped.
    #[tracing::instrument(name = "library.search_global", skip(self, sub, workspace_ids))]
    pub async fn search_global(
        &self,
        sub: &crate::auth::AuthSubject,
        workspace_ids: &[uuid::Uuid],
        query: &str,
        doc_types: &[DocType],
        limit: usize,
    ) -> Result<Vec<GlobalSearchHit>, LibraryError> {
        if matches!(sub, crate::auth::AuthSubject::Anonymous) {
            return Err(crate::auth::error::AuthorizationError::AuthenticationRequired.into());
        }
        crate::audit::Audit::record_action_if_unset("library.search");
        let effective_types: Vec<DocType> = if doc_types.is_empty() {
            vec![DocType::Skill, DocType::Note, DocType::SpaceFile]
        } else {
            doc_types
                .iter()
                .copied()
                .filter(|d| !matches!(d, DocType::Workflow))
                .collect()
        };
        if effective_types.is_empty() {
            return Ok(Vec::new());
        }
        let query_embedding = match self.embedder.embed_query(query).await {
            Ok(emb) => Some(emb),
            Err(e) => {
                tracing::warn!(error = %e, "embedding query failed, falling back to FTS-only");
                None
            }
        };
        self.search
            .search_global(
                workspace_ids,
                query,
                query_embedding,
                &effective_types,
                limit,
            )
            .await
    }

    /// Bulk lookup by id — returns full title + body for each match.
    /// Open to any non-anonymous subject (library content is globally
    /// discoverable). Missing ids are silently dropped; caller compares
    /// returned count to requested.
    #[tracing::instrument(name = "library.get_files", skip(self, sub, ids))]
    pub async fn get_files(
        &self,
        sub: &crate::auth::AuthSubject,
        ids: &[uuid::Uuid],
    ) -> Result<Vec<LibraryFile>, LibraryError> {
        if matches!(sub, crate::auth::AuthSubject::Anonymous) {
            return Err(crate::auth::error::AuthorizationError::AuthenticationRequired.into());
        }
        crate::audit::Audit::record_action_if_unset("library.get_files");
        self.search.find_by_ids(ids).await
    }
}
