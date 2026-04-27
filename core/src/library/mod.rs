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
    parse_skill_markdown, parse_workflow_yaml, DocType, GitFileHash, ParsedSkillFile,
    ParsedWorkflowFile, RuntimeFile, SearchableFields,
};
pub use job::LIBRARY_LOCK_QUEUE;
pub use search::SearchResult;

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

pub struct SkillFileChange {
    pub file: RuntimeFile,
    /// File on disk lacks proper frontmatter and should be rewritten with
    /// canonical headers after entity creation.
    pub needs_rewrite: bool,
}

pub struct SkillChanges {
    pub head_commit: String,
    pub files: Vec<SkillFileChange>,
}

/// A single changed workflow file with metadata for the sync job.
/// Mirrors [`SkillFileChange`] — the same parse + rewrite protocol
/// applies, just keyed on YAML files under `runtime/workflows/...`.
pub struct WorkflowFileChange {
    pub file: RuntimeFile,
    pub needs_rewrite: bool,
}

/// Workflow files detected as new or changed since the last sync.
pub struct WorkflowChanges {
    pub head_commit: String,
    pub files: Vec<WorkflowFileChange>,
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

    /// Upserts search data and persists an inbox event in the same transaction.
    /// The inbox handler embeds the document and spawns a serialized job for
    /// git pull/write/commit/push on any node.
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
            let idempotency_key = format!("gitkeep:{workspace_name}:{subdir}");
            let _ = self
                .inbox
                .persist_and_queue_job_in_op(op, idempotency_key, &file)
                .await?;
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
        let idempotency_key = format!("workspace-cleanup:{workspace_name}");
        let _ = self
            .inbox
            .persist_and_queue_job_in_op(op, idempotency_key, &file)
            .await?;

        Ok(())
    }

    /// On first run (`last_sync_commit` = `None`), returns all skill files.
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

    /// Mirrors [`Self::find_new_skills`] for `runtime/workflows/*.yml` and
    /// `runtime/workspaces/*/workflows/*.yml`.
    #[tracing::instrument(name = "library.find_new_workflows", skip(self))]
    pub async fn find_new_workflows(
        &self,
        last_sync_commit: Option<&str>,
    ) -> Result<WorkflowChanges, LibraryError> {
        self.upstream.pull().await?;

        let head = self.upstream.head_commit_hash().await?;
        let head = match head {
            Some(h) => h,
            None => {
                tracing::debug!("no commits in library repo");
                return Ok(WorkflowChanges {
                    head_commit: String::new(),
                    files: Vec::new(),
                });
            }
        };

        if last_sync_commit == Some(head.as_str()) {
            return Ok(WorkflowChanges {
                head_commit: head,
                files: Vec::new(),
            });
        }

        let changed = self
            .upstream
            .changed_workflow_files(last_sync_commit)
            .await?;

        let mut files = Vec::with_capacity(changed.len());
        for (path, content) in &changed {
            match file::parse_workflow_yaml(content, path) {
                Some(parsed) => {
                    files.push(WorkflowFileChange {
                        needs_rewrite: parsed.needs_rewrite,
                        file: parsed.file,
                    });
                }
                None => {
                    tracing::warn!(path = %path, "failed to parse workflow yaml, skipping");
                }
            }
        }

        Ok(WorkflowChanges {
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
