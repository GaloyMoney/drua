//! Reverse-sync of `*.md` files committed under `spaces/<slug>/` into
//! the library search index. Files are not entity-backed and never
//! written back to disk — `doc_id` is `uuidv5(SPACE_FILE_NAMESPACE,
//! "{space_id}:{relative_path}")` so the same `(space, path)` always
//! hashes to the same UUID.

use std::sync::Arc;

use job::{CurrentJob, Job, JobCompletion, JobId, JobInitializer, JobRunner, JobSpawner, JobType};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use super::Spaces;
use crate::library::{Library, LIBRARY_LOCK_QUEUE};
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
    spaces: Arc<Spaces>,
    pool: PgPool,
}

impl SpaceFilesSyncJobInitializer {
    pub fn new(library: Arc<Library>, spaces: Arc<Spaces>, pool: PgPool) -> Self {
        Self {
            library,
            spaces,
            pool,
        }
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
            spaces: Arc::clone(&self.spaces),
            pool: self.pool.clone(),
            config,
            spawner,
        }))
    }
}

struct SpaceFilesSyncRunner {
    library: Arc<Library>,
    spaces: Arc<Spaces>,
    pool: PgPool,
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
        let next_commit = match self
            .library
            .sync_space_files_once(
                &self.pool,
                &self.spaces,
                self.config.last_sync_commit.as_deref(),
            )
            .await
        {
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
        let (title, _) = extract_title_and_body(
            "# Incident playbook\n\nbody text\n",
            "runbooks/foo.md",
        );
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
        let (title, _) =
            extract_title_and_body("# \n\nbody\n", "runbooks/incident-playbook.md");
        assert_eq!(title, "incident playbook");
    }
}
