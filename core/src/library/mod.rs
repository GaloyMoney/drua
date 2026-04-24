mod error;
mod file;
mod inbox;
mod job;
mod search;
mod upstream;

use std::sync::Arc;

use crate::github_app::GitHubAppTokenProvider;

use self::inbox::LibraryWriteHandler;
use self::job::WriteToRuntimeJobInitializer;
use self::search::SearchStore;
use self::upstream::Upstream;
pub use error::LibraryError;
pub use file::{
    parse_skill_markdown, DocType, GitFileHash, ParsedSkillFile, RuntimeFile, SearchableFields,
};
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
            Some(d) => std::path::PathBuf::from(d).join("repo"),
            None => std::path::PathBuf::from(".library"),
        }
    }
}

/// A single changed skill file with metadata for the sync job.
pub struct SkillFileChange {
    /// Parsed skill file. When `needs_rewrite` is true the file's
    /// `original_path` field is set to the on-disk path.
    pub file: RuntimeFile,
    /// When `true`, the file on disk lacks proper frontmatter and should be
    /// rewritten with canonical headers after entity creation.
    pub needs_rewrite: bool,
}

/// Skill files detected as new or changed since the last sync.
pub struct SkillChanges {
    /// The HEAD commit hash after pulling.
    pub head_commit: String,
    /// Changed skill files with their original paths and rewrite flags.
    pub files: Vec<SkillFileChange>,
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

    /// Queue `.gitkeep` files for a new workspace so the folder structure
    /// (notes/ and skills/) is committed to the library repo.
    #[tracing::instrument(name = "library.sync_workspace_folder_in_op", skip_all)]
    pub async fn sync_workspace_folder_in_op(
        &self,
        op: &mut impl es_entity::AtomicOperation,
        workspace_name: &str,
    ) -> Result<(), LibraryError> {
        for subdir in ["notes", "skills"] {
            let file = RuntimeFile::GitKeep {
                workspace_name: workspace_name.to_string(),
                subdir: subdir.to_string(),
            };
            let idempotency_key = format!("gitkeep:{workspace_name}:{subdir}");
            let _ = self
                .inbox
                .persist_and_queue_job_in_op(op, idempotency_key, &file)
                .await?;
        }
        Ok(())
    }

    /// Pull the library repo and find skill files that changed since
    /// `last_sync_commit`. Returns parsed `RuntimeFile::Skill` variants.
    ///
    /// On first run (`last_sync_commit` is `None`), returns all skill files.
    /// Returns an empty `files` vec when HEAD hasn't moved.
    #[tracing::instrument(name = "library.find_new_skills", skip(self))]
    pub async fn find_new_skills(
        &self,
        last_sync_commit: Option<&str>,
    ) -> Result<SkillChanges, LibraryError> {
        self.upstream.pull().await?;

        let head = self.upstream.head_commit_hash().await?;
        let head = match head {
            Some(h) => h,
            None => {
                tracing::debug!("no commits in library repo");
                return Ok(SkillChanges {
                    head_commit: String::new(),
                    files: Vec::new(),
                });
            }
        };

        if last_sync_commit == Some(head.as_str()) {
            return Ok(SkillChanges {
                head_commit: head,
                files: Vec::new(),
            });
        }

        let changed = self.upstream.changed_skill_files(last_sync_commit).await?;

        let mut files = Vec::with_capacity(changed.len());
        for (path, content) in &changed {
            match file::parse_skill_markdown(content, path) {
                Some(parsed) => {
                    files.push(SkillFileChange {
                        needs_rewrite: parsed.needs_rewrite,
                        file: parsed.file,
                    });
                }
                None => {
                    tracing::warn!(path = %path, "failed to parse skill markdown, skipping");
                }
            }
        }

        Ok(SkillChanges {
            head_commit: head,
            files,
        })
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
