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
use self::job::WriteToRuntimeJobInitializer;
use self::search::SearchStore;
use self::upstream::Upstream;
pub use error::LibraryError;
pub(crate) use file::name_from_filename;
pub use file::{
    parse_skill_markdown, render_note_markdown, render_skill_markdown, DocType, GitFileHash,
    SearchableFields, UpstreamOp,
};
pub use job::LIBRARY_LOCK_QUEUE;
pub use search::{GlobalSearchHit, LibraryFile, SearchResult};
pub use space::Spaces;
pub(crate) use synced::slugify;
pub use synced::{
    Changes, LibraryImporter, LibrarySynced, ParsedFile, SyncFromLibraryConfig,
    SyncFromLibraryJobInitializer, SyncedFile, UpsertError,
};

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

        let handler = LibraryWriteHandler::new(search.clone(), embedder.clone(), write_spawner);
        let inbox_config = obix::InboxConfig::new(::job::JobType::new(LIBRARY_WRITE_JOB));
        let inbox = obix::Inbox::new(pool, jobs, inbox_config, handler);

        Ok(Self {
            search,
            inbox,
            embedder,
            upstream,
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

    /// Queues a `SpaceInit` op — writes `spaces/{slug}/.gitkeep` so
    /// sparse-checkout sandboxes have a directory to land in.
    #[tracing::instrument(name = "library.sync_space_folder_in_op", skip_all)]
    pub async fn sync_space_folder_in_op(
        &self,
        op: &mut impl es_entity::AtomicOperation,
        slug: &str,
    ) -> Result<(), LibraryError> {
        let file = UpstreamOp::SpaceInit {
            slug: slug.to_string(),
        };
        self.enqueue_write(op, file).await?;
        Ok(())
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
            vec![DocType::Skill, DocType::Note]
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
