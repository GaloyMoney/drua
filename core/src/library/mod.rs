mod error;
mod file;
mod job;
mod search;
mod upstream;

use std::sync::Arc;

use self::job::PushRuntimeCommitsJobInitializer;
use self::search::SearchStore;
use self::upstream::Upstream;
pub use error::LibraryError;
pub use file::{DocType, GitFileHash, RuntimeFile, SearchableFields};
pub use search::SearchResult;

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
    upstream: Upstream,
    search: SearchStore,
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

        let init = PushRuntimeCommitsJobInitializer::new(upstream.clone());
        let spawner = jobs.add_initializer(init);
        if let Err(e) = spawner
            .spawn_unique(::job::JobId::new(), PushRuntimeCommitsJobInitializer::cfg())
            .await
        {
            tracing::error!(error = %e, "Failed to spawn push-runtime-commits job");
        }

        Ok(Self {
            upstream,
            search,
            embedder,
        })
    }

    /// Upsert search data within the transaction, then write file to disk,
    /// git-commit, and fire-and-forget embedding.
    #[tracing::instrument(name = "library.write_in_op", skip_all)]
    pub async fn write_in_op(
        &self,
        op: &mut impl es_entity::AtomicOperation,
        file: &RuntimeFile,
    ) -> Result<(), LibraryError> {
        let fields = file.searchable_fields();
        self.search.upsert_in_op(op, &fields).await?;

        let relative_path = file.relative_path();
        let content = file.content();
        let commit_message = file.commit_message();

        let full_path = self.upstream.repo_path().join(&relative_path);
        if let Some(parent) = full_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| LibraryError::Io(e.to_string()))?;
        }
        tokio::fs::write(&full_path, &content)
            .await
            .map_err(|e| LibraryError::Io(e.to_string()))?;
        tracing::info!(path = %full_path.display(), "wrote runtime file");

        self.upstream
            .add_and_commit(&relative_path, &commit_message)
            .await?;

        self.spawn_embed(&fields);

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

    fn spawn_embed(&self, fields: &SearchableFields) {
        let embedder = self.embedder.clone();
        let search = self.search.clone();
        let doc_id = fields.doc_id;
        let doc_type = fields.doc_type;
        let text = format!("{}\n\n{}", fields.title, fields.body);
        tokio::spawn(async move {
            match embedder.embed_document(&text).await {
                Ok(embedding) => {
                    if let Err(e) = search.set_embedding(doc_id, doc_type, embedding).await {
                        tracing::error!(%doc_id, error = %e, "failed to store embedding");
                    }
                }
                Err(e) => {
                    tracing::error!(%doc_id, error = %e, "failed to generate embedding");
                }
            }
        });
    }
}
