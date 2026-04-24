mod error;
mod file;
mod inbox;
mod job;
mod search;
mod upstream;

use std::sync::Arc;

use self::inbox::LibraryWriteHandler;
use self::job::WriteToRuntimeJobInitializer;
use self::search::SearchStore;
use self::upstream::Upstream;
pub use error::LibraryError;
pub use file::{DocType, GitFileHash, RuntimeFile, SearchableFields};
pub use search::SearchResult;

const LIBRARY_WRITE_JOB: &str = "library.write";
const WRITE_TO_RUNTIME_JOB: &str = "library.write-to-runtime";

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct LibraryConfig {
    /// Local path to clone the library repo into.
    /// Defaults to `<repo-root>/.library/`.
    #[serde(default)]
    pub data_dir: Option<String>,
    /// Git remote URL to clone from.
    #[serde(default)]
    pub repo_url: Option<String>,
}

impl LibraryConfig {
    pub fn repo_path(&self) -> std::path::PathBuf {
        match &self.data_dir {
            Some(d) => std::path::PathBuf::from(d),
            None => std::path::PathBuf::from(".library"),
        }
    }
}

#[derive(Clone)]
pub struct Library {
    search: SearchStore,
    inbox: obix::Inbox,
    embedder: Arc<code_assistant_core::embedder::Embedder>,
}

impl Library {
    pub async fn init(
        config: &LibraryConfig,
        pool: &sqlx::PgPool,
        embedder: Arc<code_assistant_core::embedder::Embedder>,
        jobs: &mut ::job::Jobs,
    ) -> Result<Self, LibraryError> {
        let upstream = Upstream::init(config.repo_url.as_deref(), config.repo_path()).await?;
        let search = SearchStore::new(pool);

        let write_init = WriteToRuntimeJobInitializer::new(upstream);
        let write_spawner = jobs.add_initializer(write_init);

        let handler = LibraryWriteHandler::new(search.clone(), embedder.clone(), write_spawner);
        let inbox_config = obix::InboxConfig::new(::job::JobType::new(LIBRARY_WRITE_JOB));
        let inbox = obix::Inbox::new(pool, jobs, inbox_config, handler);

        Ok(Self {
            search,
            inbox,
            embedder,
        })
    }

    /// Upsert search data and persist an inbox event within the transaction.
    /// The inbox handler will embed the document and spawn a serialized job
    /// for git pull/write/commit/push on any node.
    #[tracing::instrument(name = "library.write_in_op", skip_all)]
    pub async fn write_in_op(
        &self,
        op: &mut impl es_entity::AtomicOperation,
        file: &RuntimeFile,
    ) -> Result<(), LibraryError> {
        if let Some(fields) = file.searchable_fields() {
            self.search.upsert_in_op(op, &fields).await?;
        }

        let idempotency_key = file.file_hash().to_string();
        let _ = self
            .inbox
            .persist_and_queue_job_in_op(op, idempotency_key, file)
            .await?;

        Ok(())
    }

    /// Queue a `.gitkeep` file for a new workspace so the folder structure
    /// is committed to the library repo.
    #[tracing::instrument(name = "library.sync_workspace_folder_in_op", skip_all)]
    pub async fn sync_workspace_folder_in_op(
        &self,
        op: &mut impl es_entity::AtomicOperation,
        workspace_name: &str,
    ) -> Result<(), LibraryError> {
        let file = RuntimeFile::GitKeep {
            workspace_name: workspace_name.to_string(),
        };
        let idempotency_key = file.file_hash().to_string();
        let _ = self
            .inbox
            .persist_and_queue_job_in_op(op, idempotency_key, &file)
            .await?;
        Ok(())
    }

    /// Hybrid search across library documents.
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
