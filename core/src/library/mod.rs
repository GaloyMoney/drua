mod error;
mod file;
mod inbox;
mod job;
mod search;
mod synced;
mod upstream;

use std::sync::Arc;

use crate::github_app::GitHubAppTokenProvider;

use self::inbox::LibraryWriteHandler;
use self::job::WriteToRuntimeJobInitializer;
use self::search::SearchStore;
use self::upstream::Upstream;
pub use error::LibraryError;
pub use file::{
    parse_skill_markdown, parse_workflow_yaml, render_note_markdown, render_skill_markdown,
    render_workflow_yaml, DocType, GitFileHash, ParsedWorkflowFile, RuntimeFile, SearchableFields,
};
pub use job::LIBRARY_LOCK_QUEUE;
pub use search::SearchResult;
pub use synced::{
    sync_to_library, Changes, LibraryFileFormat, LibrarySynced, ParsedFile, SyncFromLibraryConfig,
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

    /// Registers a `LibrarySyncHook` on `op` so that — at commit time — the
    /// file's search row is upserted and an inbox event is persisted in the
    /// same transaction. Multiple registrations within one op merge into a
    /// single hook (one inbox row, one batched search pass).
    ///
    /// The inbox handler then embeds the document and spawns a serialized
    /// `WriteToRuntime` job for git pull/write/commit/push on any node.
    #[tracing::instrument(name = "library.write_in_op", skip_all)]
    pub async fn write_in_op(
        &self,
        op: &mut impl es_entity::AtomicOperation,
        file: &RuntimeFile,
    ) -> Result<(), LibraryError> {
        synced::enqueue_write(op, &self.inbox, &self.search, file.clone()).await?;
        Ok(())
    }

    /// Queues `.gitkeep` files so notes/, skills/, workflows/ folders are committed.
    #[tracing::instrument(name = "library.sync_workspace_folder_in_op", skip_all)]
    pub async fn sync_workspace_folder_in_op(
        &self,
        op: &mut impl es_entity::AtomicOperation,
        workspace_name: &str,
    ) -> Result<(), LibraryError> {
        for subdir in ["notes", "skills", "workflows"] {
            let file = RuntimeFile::GitKeep {
                workspace_name: workspace_name.to_string(),
                subdir: subdir.to_string(),
            };
            synced::enqueue_write(op, &self.inbox, &self.search, file).await?;
        }
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

        let file = RuntimeFile::WorkspaceCleanup {
            workspace_name: workspace_name.to_string(),
        };
        synced::enqueue_write(op, &self.inbox, &self.search, file).await?;
        Ok(())
    }

    /// Generic reverse-sync. Returns parsed `ParsedFile`s for every changed
    /// file under `E::DOC_TYPE`'s subdir since `last_sync_commit`. On first
    /// run (`None`), returns all tracked files. Empty `files` when HEAD
    /// hasn't moved.
    #[tracing::instrument(name = "library.find_changes", skip(self))]
    pub async fn find_changes<E: LibraryFileFormat>(
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

        let changed = self
            .upstream
            .changed_files(last_sync_commit, E::DOC_TYPE.subdir(), E::DOC_TYPE.ext())
            .await?;

        let mut files = Vec::with_capacity(changed.len());
        for (path, content) in &changed {
            match E::parse(content, path) {
                Some(parsed) => files.push(parsed),
                None => {
                    tracing::warn!(
                        path = %path,
                        doc_type = E::DOC_TYPE.as_str(),
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
}
