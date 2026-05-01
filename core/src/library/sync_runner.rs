use std::sync::Arc;

use job::{CurrentJob, Job, JobCompletion, JobId, JobInitializer, JobRunner, JobSpawner, JobType};
use serde::{Deserialize, Serialize};

use super::git::{CommitOid, Delta};
use super::importer::ImporterRegistry;
use super::{DocType, Library, LibraryError, LIBRARY_LOCK_QUEUE};
use crate::primitives::ProjectId;
use crate::project::Projects;

pub const LIBRARY_SYNC_JOB: &str = "library.unified-sync";

#[derive(Debug, Serialize, Deserialize)]
pub struct LibrarySyncConfig {
    pub sync_interval_secs: u64,
    #[serde(default)]
    pub last_sync_commit: Option<String>,
}

pub struct LibrarySyncJobInitializer {
    library: Arc<Library>,
    registry: Arc<ImporterRegistry>,
    projects: Arc<Projects>,
}

impl LibrarySyncJobInitializer {
    pub fn new(
        library: Arc<Library>,
        registry: Arc<ImporterRegistry>,
        projects: Arc<Projects>,
    ) -> Self {
        Self {
            library,
            registry,
            projects,
        }
    }
}

impl JobInitializer for LibrarySyncJobInitializer {
    type Config = LibrarySyncConfig;

    fn job_type(&self) -> JobType {
        JobType::new(LIBRARY_SYNC_JOB)
    }

    fn init(
        &self,
        job: &Job,
        spawner: JobSpawner<Self::Config>,
    ) -> Result<Box<dyn JobRunner>, Box<dyn std::error::Error>> {
        let config: LibrarySyncConfig = job.config()?;
        Ok(Box::new(LibrarySyncRunner {
            library: Arc::clone(&self.library),
            registry: Arc::clone(&self.registry),
            projects: Arc::clone(&self.projects),
            config,
            spawner,
        }))
    }
}

struct LibrarySyncRunner {
    library: Arc<Library>,
    registry: Arc<ImporterRegistry>,
    projects: Arc<Projects>,
    config: LibrarySyncConfig,
    spawner: JobSpawner<LibrarySyncConfig>,
}

impl LibrarySyncRunner {
    async fn pump_once(&self, current_job: &CurrentJob) -> Result<Option<String>, LibraryError> {
        self.library.git().fetch().await?;
        let head = match self.library.git().head().await? {
            Some(h) => h,
            None => return Ok(None),
        };
        let last = self
            .config
            .last_sync_commit
            .as_deref()
            .and_then(|s| CommitOid::from_hex(s).ok());
        if last == Some(head) {
            return Ok(Some(head.to_hex()));
        }

        let diff = self.library.git().tree_diff(last, head).await?;
        if diff.deltas.is_empty() {
            return Ok(Some(head.to_hex()));
        }

        tracing::info!(
            count = diff.deltas.len(),
            head = %head.to_hex(),
            "processing changed files"
        );

        let mut ws_cache: std::collections::HashMap<String, Option<ProjectId>> =
            std::collections::HashMap::new();

        let mut op = current_job.begin_op().await?;
        for delta in diff.deltas {
            let path = match &delta {
                Delta::Upserted { path } | Delta::Deleted { path } => path.clone(),
            };
            let Some(importer) = self.registry.dispatch_for(&path) else {
                tracing::debug!(path, "no importer matches; skipping");
                continue;
            };

            match delta {
                Delta::Upserted { .. } => {
                    let bytes = match self.library.git().read_blob_at(head, &path).await? {
                        Some(b) => b,
                        None => continue,
                    };
                    let Some(parsed) = importer.parse(&bytes, &path) else {
                        tracing::warn!(
                            path,
                            doc_type = importer.doc_type().as_str(),
                            "parse returned None"
                        );
                        continue;
                    };

                    // SpaceFile carries slug-as-resolution-token in
                    // project_name; skip the project lookup for it.
                    let project_id = if importer.doc_type() == DocType::SpaceFile {
                        None
                    } else {
                        match parsed.file.project_name.as_deref() {
                            Some(name) => match ws_cache.get(name).copied() {
                                Some(c) => c,
                                None => {
                                    let id = match self.projects.find_by_name(name).await {
                                        Ok(Some(p)) => Some(p.id),
                                        Ok(None) => {
                                            tracing::warn!(project_name = %name, "project not found");
                                            None
                                        }
                                        Err(e) => {
                                            tracing::warn!(project_name = %name, error = %e, "project lookup failed");
                                            None
                                        }
                                    };
                                    ws_cache.insert(name.to_string(), id);
                                    id
                                }
                            },
                            None => None,
                        }
                    };

                    if importer.project_required() && project_id.is_none() {
                        continue;
                    }
                    let hash = parsed.file.file_hash();
                    if let Err(e) = importer
                        .upsert(&mut op, &parsed.file, &path, project_id, hash)
                        .await
                    {
                        tracing::warn!(error = %e, path, "upsert failed");
                    }
                }
                Delta::Deleted { .. } => {
                    if let Err(e) = importer.delete(&mut op, &path).await {
                        tracing::warn!(error = %e, path, "delete failed");
                    }
                }
            }
        }
        op.commit().await?;
        Ok(Some(head.to_hex()))
    }
}

#[async_trait::async_trait]
impl JobRunner for LibrarySyncRunner {
    #[tracing::instrument(name = "library.unified_sync.run", skip_all)]
    async fn run(
        &self,
        current_job: CurrentJob,
    ) -> Result<JobCompletion, Box<dyn std::error::Error>> {
        let next_commit = match self.pump_once(&current_job).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "unified sync cycle failed");
                self.config.last_sync_commit.clone()
            }
        };
        let next_config = LibrarySyncConfig {
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
