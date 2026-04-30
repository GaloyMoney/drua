//! Reverse-sync of `*.md` files committed under `spaces/<slug>/` into
//! the library search index. Files are not entity-backed and never
//! written back to disk — `doc_id` is `uuidv5(SPACE_FILE_NAMESPACE,
//! "{space_id}:{relative_path}")` so the same `(space, path)` always
//! hashes to the same UUID.
//!
//! All sync work lives in this module: the runner holds an
//! `Arc<Library>` and reads everything it needs (upstream, search,
//! embedder, spaces, pool) through `pub(in crate::library)` accessors
//! on `Library`.

use std::sync::Arc;

use job::{CurrentJob, Job, JobCompletion, JobId, JobInitializer, JobRunner, JobSpawner, JobType};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::library::{
    DocType, GitFileHash, Library, LibraryError, SearchableFields, SpaceFileChange,
    LIBRARY_LOCK_QUEUE,
};
use crate::primitives::SpaceId;

pub const SPACE_FILES_SYNC_JOB: &str = "library.sync-space-files";

/// Frozen namespace UUID — changing it would invalidate every existing
/// `space_search_data` row's identity.
const SPACE_FILE_NAMESPACE: Uuid = Uuid::from_u128(0x6c4d339d_2184_4fa9_9f12_6e375b8291ae);

/// Deterministic `doc_id` for a space file. Idempotent re-imports rely
/// on this: same `(space, path)` always hashes the same UUID.
pub fn doc_id_for(space_id: SpaceId, relative_path: &str) -> Uuid {
    let key = format!("{}:{relative_path}", uuid::Uuid::from(space_id));
    Uuid::new_v5(&SPACE_FILE_NAMESPACE, key.as_bytes())
}

/// First H1 line wins; falls back to filename stem with `-`/`_` → spaces.
pub(crate) fn extract_title_and_body(content: &str, path: &str) -> (String, String) {
    for line in content.lines().take(20) {
        if let Some(title) = line.trim_start().strip_prefix("# ") {
            let title = title.trim();
            if !title.is_empty() {
                return (title.to_string(), content.to_string());
            }
        }
    }
    let fallback = std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.replace(['-', '_'], " "))
        .unwrap_or_else(|| path.to_string());
    (fallback, content.to_string())
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SpaceFilesSyncConfig {
    pub sync_interval_secs: u64,
    #[serde(default)]
    pub last_sync_commit: Option<String>,
}

pub struct SpaceFilesSyncJobInitializer {
    library: Arc<Library>,
}

impl SpaceFilesSyncJobInitializer {
    pub fn new(library: Arc<Library>) -> Self {
        Self { library }
    }
}

impl JobInitializer for SpaceFilesSyncJobInitializer {
    type Config = SpaceFilesSyncConfig;

    fn job_type(&self) -> JobType {
        JobType::new(SPACE_FILES_SYNC_JOB)
    }

    fn init(
        &self,
        job: &Job,
        spawner: JobSpawner<Self::Config>,
    ) -> Result<Box<dyn JobRunner>, Box<dyn std::error::Error>> {
        let config: SpaceFilesSyncConfig = job.config()?;
        Ok(Box::new(SpaceFilesSyncRunner {
            library: Arc::clone(&self.library),
            config,
            spawner,
        }))
    }
}

struct SpaceFilesSyncRunner {
    library: Arc<Library>,
    config: SpaceFilesSyncConfig,
    spawner: JobSpawner<SpaceFilesSyncConfig>,
}

#[async_trait::async_trait]
impl JobRunner for SpaceFilesSyncRunner {
    #[tracing::instrument(name = "library.space_files_sync.run", skip_all)]
    async fn run(
        &self,
        _current_job: CurrentJob,
    ) -> Result<JobCompletion, Box<dyn std::error::Error>> {
        let next_commit =
            match sync_once(&self.library, self.config.last_sync_commit.as_deref()).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "space-file sync cycle failed");
                    self.config.last_sync_commit.clone()
                }
            };

        let next_config = SpaceFilesSyncConfig {
            sync_interval_secs: self.config.sync_interval_secs,
            last_sync_commit: next_commit,
        };
        let schedule_at =
            chrono::Utc::now() + chrono::Duration::seconds(self.config.sync_interval_secs as i64);
        self.spawner
            .spawn_at_with_queue_id(JobId::new(), next_config, schedule_at, LIBRARY_LOCK_QUEUE)
            .await?;
        Ok(JobCompletion::Complete)
    }
}

/// One pass: pulls upstream, walks `spaces/<slug>/**/*.md` since
/// `last_commit`, and upserts/deletes `library_search_data` +
/// `space_search_data` rows. Returns the new HEAD commit.
async fn sync_once(
    library: &Library,
    last_commit: Option<&str>,
) -> Result<Option<String>, LibraryError> {
    library.upstream().pull().await?;
    let head = match library.upstream().head_commit_hash().await? {
        Some(h) => h,
        None => return Ok(None),
    };
    if last_commit == Some(head.as_str()) {
        return Ok(Some(head));
    }

    let changes = library.upstream().changed_space_files(last_commit).await?;
    for change in changes {
        if let Err(e) = apply_change(library, change).await {
            tracing::warn!(error = %e, "failed to apply space-file change");
        }
    }
    Ok(Some(head))
}

async fn apply_change(library: &Library, change: SpaceFileChange) -> Result<(), LibraryError> {
    match change {
        SpaceFileChange::Upserted {
            slug,
            relative_path,
            content,
        } => apply_upsert(library, slug, relative_path, content).await,
        SpaceFileChange::Deleted {
            slug,
            relative_path,
        } => apply_delete(library, slug, relative_path).await,
    }
}

async fn apply_upsert(
    library: &Library,
    slug: String,
    relative_path: String,
    content: String,
) -> Result<(), LibraryError> {
    let Some(space) = library.find_space_by_slug(&slug).await? else {
        tracing::warn!(%slug, "no space matches slug, skipping file");
        return Ok(());
    };
    let doc_id = doc_id_for(space.id, &relative_path);
    let content_hash = GitFileHash::from_blob_bytes(content.as_bytes());
    let (title, body) = extract_title_and_body(&content, &relative_path);

    let upserted = library
        .search_store()
        .upsert_space_file_if_changed(
            &SearchableFields {
                doc_id,
                doc_type: DocType::SpaceFile,
                // Space files aren't workspace-scoped — `nil()` reflects
                // "library-global" in the FTS index.
                workspace_id: uuid::Uuid::nil(),
                title: title.clone(),
                body: body.clone(),
                tags: Vec::new(),
            },
            space.id,
            &relative_path,
            &content_hash,
        )
        .await?;
    if !upserted {
        tracing::debug!(%slug, %relative_path, "space file unchanged, skipping");
        return Ok(());
    }

    // Embedding is best-effort; FTS index works without it.
    let text = format!("{title}\n\n{body}");
    match library.embedder().embed_document(&text).await {
        Ok(emb) => {
            if let Err(e) = library
                .search_store()
                .set_embedding(doc_id, DocType::SpaceFile, emb)
                .await
            {
                tracing::warn!(error = %e, %slug, %relative_path, "set_embedding failed");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, %slug, %relative_path, "embed_document failed");
        }
    }
    Ok(())
}

async fn apply_delete(
    library: &Library,
    slug: String,
    relative_path: String,
) -> Result<(), LibraryError> {
    let Some(space) = library.find_space_by_slug(&slug).await? else {
        return Ok(());
    };
    let doc_id = doc_id_for(space.id, &relative_path);
    library.search_store().delete_space_file(doc_id).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_id_is_deterministic() {
        let space = SpaceId::new();
        let a = doc_id_for(space, "runbooks/foo.md");
        let b = doc_id_for(space, "runbooks/foo.md");
        assert_eq!(a, b);
    }

    #[test]
    fn doc_id_changes_with_path() {
        let space = SpaceId::new();
        let a = doc_id_for(space, "runbooks/foo.md");
        let b = doc_id_for(space, "runbooks/bar.md");
        assert_ne!(a, b);
    }

    #[test]
    fn doc_id_changes_with_space() {
        let path = "runbooks/foo.md";
        let a = doc_id_for(SpaceId::new(), path);
        let b = doc_id_for(SpaceId::new(), path);
        assert_ne!(a, b);
    }

    #[test]
    fn extract_title_uses_first_h1() {
        let (title, _) =
            extract_title_and_body("# Incident playbook\n\nbody text\n", "runbooks/foo.md");
        assert_eq!(title, "Incident playbook");
    }

    #[test]
    fn extract_title_falls_back_to_filename() {
        let (title, _) = extract_title_and_body(
            "no heading here\nbody text\n",
            "runbooks/incident-playbook.md",
        );
        assert_eq!(title, "incident playbook");
    }

    #[test]
    fn extract_title_skips_empty_h1() {
        let (title, _) = extract_title_and_body("# \n\nbody\n", "runbooks/incident-playbook.md");
        assert_eq!(title, "incident playbook");
    }
}
