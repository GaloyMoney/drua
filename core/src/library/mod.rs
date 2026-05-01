mod error;
mod file;
mod inbox;
mod job;
mod search;
pub mod space;
mod space_fs;
mod space_path;
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
pub use space_fs::{FileView, SpaceFs};
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
            write_spawner,
            jobs: jobs.clone(),
        })
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

    /// Queues a single `ProjectInit` op — `notes/`, `skills/`,
    /// `workflows/` `.gitkeep` markers all land in one commit + push.
    #[tracing::instrument(name = "library.sync_project_folder_in_op", skip_all)]
    pub async fn sync_project_folder_in_op(
        &self,
        op: &mut impl es_entity::AtomicOperation,
        project_name: &str,
    ) -> Result<(), LibraryError> {
        let file = UpstreamOp::ProjectInit {
            project_name: project_name.to_string(),
        };
        self.enqueue_write(op, file).await?;
        Ok(())
    }

    /// Persists a new `Space` and queues `spaces/<slug>/.gitkeep` for
    /// upstream commit in the same transaction. Mounting is decoupled
    /// from creation — callers that want the creating project to see
    /// the space follow up with `Projects::mount_space` (or use the
    /// combined `Projects::create_and_mount_space`).
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
        let mut builder = NewSpace::builder().slug(slug.into());
        if let Some(desc) = description {
            builder = builder.description(desc);
        }
        let new_space = builder.build()?;
        let space = self.space_repo.create_in_op(&mut op, new_space).await?;

        // Spawn the upstream `.gitkeep` commit with a known `JobId`.
        // Persisted in the same `op`, so the spawn is durable: a crash
        // before commit drops both the entity and the job; a crash
        // after picks the job up from the persisted queue.
        let job_id = ::job::JobId::new();
        let config = WriteToRuntimeConfig {
            file: UpstreamOp::SpaceInit {
                slug: space.slug.clone(),
            },
        };
        self.write_spawner
            .spawn_with_queue_id_in_op(&mut op, job_id, config, LIBRARY_LOCK_QUEUE)
            .await?;
        op.commit().await?;

        // Block until the upstream `spaces/<slug>/.gitkeep` commit
        // lands so callers can rely on the directory existing once
        // `create_space` returns.
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

        tracing::info!(space.id = %space.id, space.slug = %space.slug, "space created");
        Ok(space)
    }

    /// Blind overwrite of `spaces/<slug>/<relative_path>`. Awaits the
    /// upstream push so callers can rely on the file being at the
    /// remote when this returns.
    ///
    /// Trusted-caller: mount-membership authz lives in `Projects`
    /// (`Project.mounted_spaces`), not on `AuthScope`. `SpaceFs` is
    /// the public boundary and gates every call via
    /// `Projects::space_for_subject`. Visibility is `pub(in crate::library)`
    /// so external code can't bypass that gate.
    #[tracing::instrument(name = "library.write_space_file", skip(self, space, content), fields(slug = %space.slug))]
    pub(in crate::library) async fn write_space_file(
        &self,
        space: &Space,
        relative_path: String,
        content: String,
    ) -> Result<(), LibraryError> {
        crate::audit::Audit::record_action_if_unset("space.write_file");
        self.spawn_and_await_space_op(
            &space.slug,
            UpstreamOp::SpaceFileWrite {
                slug: space.slug.clone(),
                relative_path,
                content,
            },
        )
        .await
    }

    /// Removes `spaces/<slug>/<relative_path>` from the library repo.
    /// No-op if the file doesn't exist on disk. Trusted-caller — see
    /// [`Library::write_space_file`].
    #[tracing::instrument(name = "library.delete_space_file", skip(self, space), fields(slug = %space.slug))]
    pub(in crate::library) async fn delete_space_file(
        &self,
        space: &Space,
        relative_path: String,
    ) -> Result<(), LibraryError> {
        crate::audit::Audit::record_action_if_unset("space.delete_file");
        self.spawn_and_await_space_op(
            &space.slug,
            UpstreamOp::SpaceFileDelete {
                slug: space.slug.clone(),
                relative_path,
            },
        )
        .await
    }

    /// Read–modify–write at the worker. Verifies `old_str` appears
    /// exactly once on the freshest disk state before substituting.
    /// Trusted-caller — see [`Library::write_space_file`].
    #[tracing::instrument(name = "library.str_replace_space_file", skip(self, space, old_str, new_str), fields(slug = %space.slug))]
    pub(in crate::library) async fn str_replace_space_file(
        &self,
        space: &Space,
        relative_path: String,
        old_str: String,
        new_str: String,
    ) -> Result<(), LibraryError> {
        crate::audit::Audit::record_action_if_unset("space.str_replace");
        self.spawn_and_await_space_op(
            &space.slug,
            UpstreamOp::SpaceFileStrReplace {
                slug: space.slug.clone(),
                relative_path,
                old_str,
                new_str,
            },
        )
        .await
    }

    /// Read–modify–write at the worker. Inserts `text` after
    /// `line_number` (1-based, `0` = beginning of file). Out-of-range
    /// line numbers append at EOF. Trusted-caller — see
    /// [`Library::write_space_file`].
    #[tracing::instrument(name = "library.insert_space_file", skip(self, space, text), fields(slug = %space.slug))]
    pub(in crate::library) async fn insert_space_file(
        &self,
        space: &Space,
        relative_path: String,
        line_number: usize,
        text: String,
    ) -> Result<(), LibraryError> {
        crate::audit::Audit::record_action_if_unset("space.insert");
        self.spawn_and_await_space_op(
            &space.slug,
            UpstreamOp::SpaceFileInsert {
                slug: space.slug.clone(),
                relative_path,
                line_number,
                text,
            },
        )
        .await
    }

    /// Renames `spaces/<slug>/<from>` → `spaces/<slug>/<to>`. Worker
    /// rejects same-path, missing src, or existing dest. Cross-space
    /// moves are blocked at `SpaceFs::move_file` — both sides must
    /// resolve to the same `Space`. Trusted-caller — see
    /// [`Library::write_space_file`].
    #[tracing::instrument(name = "library.move_space_file", skip(self, space), fields(slug = %space.slug))]
    pub(in crate::library) async fn move_space_file(
        &self,
        space: &Space,
        from_relative_path: String,
        to_relative_path: String,
    ) -> Result<(), LibraryError> {
        crate::audit::Audit::record_action_if_unset("space.move_file");
        self.spawn_and_await_space_op(
            &space.slug,
            UpstreamOp::SpaceFileMove {
                slug: space.slug.clone(),
                from_relative_path,
                to_relative_path,
            },
        )
        .await
    }

    /// Spawns a single space-related `UpstreamOp` on the
    /// `LIBRARY_LOCK_QUEUE` and blocks until it completes (or the
    /// 30s timeout fires). Used by the four space-file write helpers
    /// so callers can rely on "returned == pushed".
    async fn spawn_and_await_space_op(
        &self,
        slug: &str,
        op: UpstreamOp,
    ) -> Result<(), LibraryError> {
        const SPACE_OP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
        let job_id = ::job::JobId::new();
        let config = WriteToRuntimeConfig { file: op };
        self.write_spawner
            .spawn_with_queue_id(job_id, config, LIBRARY_LOCK_QUEUE)
            .await?;
        let outcomes = self
            .jobs
            .await_completions(&[job_id], Some(SPACE_OP_TIMEOUT))
            .await?;
        if !outcomes.iter().all(|o| o.is_completed()) {
            return Err(LibraryError::SpaceOpFailed {
                slug: slug.to_string(),
            });
        }
        Ok(())
    }

    /// Lists every space, paginated. Soft-deleted spaces drop out
    /// at the SQL layer (`delete = "soft_without_queries"`). Used by
    /// admin tools and the `spaces` `list { all: true }` flag.
    #[tracing::instrument(name = "library.list_all_spaces", skip(self, sub))]
    pub async fn list_all_spaces(
        &self,
        sub: &crate::auth::AuthSubject,
    ) -> Result<Vec<Space>, LibraryError> {
        sub.can(
            crate::auth::AuthVerb::Read,
            crate::auth::AuthResource::Space(None),
        )?;
        crate::audit::Audit::record_action_if_unset("space.list_all");
        Ok(self.space_repo.list_all().await?)
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

    /// On-disk root for `spaces/<slug>/` inside the library clone.
    /// `SpaceFs` reads from here directly; the path is not validated
    /// against `Space` existence — callers must run
    /// `Projects::space_for_subject` first.
    pub fn space_root(&self, slug: &str) -> std::path::PathBuf {
        self.upstream.repo_path().join("spaces").join(slug)
    }

    /// No-auth slug → `Space` lookup. Soft-deleted entries are filtered
    /// by the underlying repo. Authorization in the spaces model lives
    /// on `Project.mounted_spaces`, not the space row — callers must
    /// enforce mount membership via `Projects::space_for_subject`
    /// before treating the result as visible to a subject.
    #[tracing::instrument(name = "library.find_space_by_slug", skip(self))]
    pub async fn find_space_by_slug(&self, slug: &str) -> Result<Option<Space>, LibraryError> {
        Ok(self.space_repo.maybe_find_by_slug(slug).await?)
    }

    /// Bulk hydration. Soft-deleted ids silently drop out. Order of
    /// the returned `Vec` is not aligned with the input slice.
    #[tracing::instrument(name = "library.find_spaces_by_ids", skip(self, ids))]
    pub async fn find_spaces_by_ids(
        &self,
        ids: &[crate::primitives::SpaceId],
    ) -> Result<Vec<Space>, LibraryError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let map = self.space_repo.find_all::<Space>(ids).await?;
        Ok(map.into_values().collect())
    }

    /// Removes search data and queues a job to delete
    /// `runtime/projects/<name>/` from the library repo. Call inside the
    /// project delete transaction for atomicity.
    #[tracing::instrument(name = "library.cleanup_project_in_op", skip_all)]
    pub async fn cleanup_project_in_op(
        &self,
        op: &mut impl es_entity::AtomicOperation,
        project_id: uuid::Uuid,
        project_name: &str,
    ) -> Result<(), LibraryError> {
        self.search.delete_for_project_in_op(op, project_id).await?;

        let file = UpstreamOp::ProjectCleanup {
            project_name: project_name.to_string(),
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
        project_id: uuid::Uuid,
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
            .search(project_id, query, query_embedding, doc_type, limit)
            .await
    }

    /// Cross-project search. Open to any non-anonymous subject;
    /// library content is globally discoverable. Empty `project_ids`
    /// = no project filter (every project plus global content);
    /// otherwise hits are restricted to the supplied ids (plus the nil
    /// UUID for global, auto-appended).
    ///
    /// Workflows are hosted in the library repo but excluded from search:
    /// passing an empty `doc_types` defaults to `[Skill, Note]`, and any
    /// `Workflow` entry in `doc_types` is silently dropped.
    #[tracing::instrument(name = "library.search_global", skip(self, sub, project_ids))]
    pub async fn search_global(
        &self,
        sub: &crate::auth::AuthSubject,
        project_ids: &[uuid::Uuid],
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
            .search_global(project_ids, query, query_embedding, &effective_types, limit)
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
