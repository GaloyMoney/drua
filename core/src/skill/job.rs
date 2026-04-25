use std::sync::Arc;

use job::*;
use serde::{Deserialize, Serialize};

use crate::library::{Library, RuntimeFile, SkillFileChange, LIBRARY_LOCK_QUEUE};
use crate::workspace::Workspaces;

use super::Skills;

pub(crate) const SYNC_SKILLS_FROM_LIBRARY_JOB: &str = "skill.sync-from-library";

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SyncSkillsFromLibraryConfig {
    pub sync_interval_secs: u64,
    #[serde(default)]
    #[allow(clippy::option_option)]
    pub last_sync_commit: Option<String>,
}

impl SyncSkillsFromLibraryConfig {
    fn next(&self, last_sync_commit: Option<String>) -> Self {
        Self {
            sync_interval_secs: self.sync_interval_secs,
            last_sync_commit,
        }
    }
}

pub(crate) struct SyncSkillsFromLibraryJobInitializer {
    library: Library,
    skills: Arc<Skills>,
    workspaces: Arc<Workspaces>,
}

impl SyncSkillsFromLibraryJobInitializer {
    pub fn new(library: Library, skills: Arc<Skills>, workspaces: Arc<Workspaces>) -> Self {
        Self {
            library,
            skills,
            workspaces,
        }
    }
}

impl JobInitializer for SyncSkillsFromLibraryJobInitializer {
    type Config = SyncSkillsFromLibraryConfig;

    fn job_type(&self) -> JobType {
        JobType::new(SYNC_SKILLS_FROM_LIBRARY_JOB)
    }

    fn init(
        &self,
        job: &Job,
        spawner: JobSpawner<Self::Config>,
    ) -> Result<Box<dyn JobRunner>, Box<dyn std::error::Error>> {
        let config: SyncSkillsFromLibraryConfig = job.config()?;
        Ok(Box::new(SyncSkillsFromLibraryRunner {
            library: self.library.clone(),
            skills: Arc::clone(&self.skills),
            workspaces: Arc::clone(&self.workspaces),
            config,
            spawner,
        }))
    }
}

struct SyncSkillsFromLibraryRunner {
    library: Library,
    skills: Arc<Skills>,
    workspaces: Arc<Workspaces>,
    config: SyncSkillsFromLibraryConfig,
    spawner: JobSpawner<SyncSkillsFromLibraryConfig>,
}

#[async_trait::async_trait]
impl JobRunner for SyncSkillsFromLibraryRunner {
    #[tracing::instrument(name = "skill.sync_from_library.run", skip_all)]
    async fn run(
        &self,
        current_job: CurrentJob,
    ) -> Result<JobCompletion, Box<dyn std::error::Error>> {
        let next_commit = match self.sync_once(&current_job).await {
            Ok(commit) => commit,
            Err(e) => {
                tracing::warn!(error = %e, "skill sync cycle failed");
                self.config.last_sync_commit.clone()
            }
        };

        // Schedule the next sync on the same serialized queue.
        let next_config = self.config.next(next_commit);
        let schedule_at =
            chrono::Utc::now() + chrono::Duration::seconds(self.config.sync_interval_secs as i64);
        self.spawner
            .spawn_at_with_queue_id(JobId::new(), next_config, schedule_at, LIBRARY_LOCK_QUEUE)
            .await?;

        Ok(JobCompletion::Complete)
    }
}

impl SyncSkillsFromLibraryRunner {
    /// Run a single sync cycle. Returns the new HEAD commit hash to carry
    /// forward into the next scheduled job.
    async fn sync_once(
        &self,
        current_job: &CurrentJob,
    ) -> Result<Option<String>, Box<dyn std::error::Error>> {
        let changes = self
            .library
            .find_new_skills(self.config.last_sync_commit.as_deref())
            .await?;

        if changes.files.is_empty() {
            let commit = if changes.head_commit.is_empty() {
                None
            } else {
                Some(changes.head_commit)
            };
            return Ok(commit);
        }

        tracing::info!(
            count = changes.files.len(),
            head = %changes.head_commit,
            "processing changed skill files from library"
        );

        // Resolve workspace IDs outside the transaction (read-only lookups).
        // Dedup by workspace_name to avoid repeated DB queries for the same workspace.
        let mut ws_cache: std::collections::HashMap<
            String,
            Option<crate::primitives::WorkspaceId>,
        > = std::collections::HashMap::new();
        let mut resolved: Vec<(&SkillFileChange, Option<crate::primitives::WorkspaceId>)> =
            Vec::with_capacity(changes.files.len());
        for change in &changes.files {
            let ws_id = match Self::workspace_name(&change.file) {
                Some(name) => match ws_cache.get(name) {
                    Some(cached) => *cached,
                    None => {
                        let id = self.resolve_workspace_id(name).await;
                        ws_cache.insert(name.to_string(), id);
                        id
                    }
                },
                None => None,
            };
            resolved.push((change, ws_id));
        }

        // Batch upsert all skills in a single transaction.
        let mut op = current_job.begin_op().await?;
        for (change, ws_id) in &resolved {
            let file_hash = change.file.file_hash();
            if let Err(e) = self
                .skills
                .upsert_from_library_in_op(&mut op, &change.file, *ws_id, file_hash)
                .await
            {
                tracing::warn!(error = %e, "failed to upsert skill from library, skipping");
            }
        }
        op.commit().await?;

        Ok(Some(changes.head_commit))
    }

    /// Extract workspace_name from a RuntimeFile.
    fn workspace_name(file: &RuntimeFile) -> Option<&str> {
        match file {
            RuntimeFile::Skill { workspace_name, .. } => workspace_name.as_deref(),
            _ => None,
        }
    }

    /// Resolve workspace_id from a workspace name.
    async fn resolve_workspace_id(&self, ws_name: &str) -> Option<crate::primitives::WorkspaceId> {
        match self.workspaces.find_by_name(ws_name).await {
            Ok(Some(ws)) => Some(ws.id),
            Ok(None) => {
                tracing::warn!(
                    workspace_name = %ws_name,
                    "workspace not found for skill file, treating as global"
                );
                None
            }
            Err(e) => {
                tracing::warn!(
                    workspace_name = %ws_name,
                    error = %e,
                    "failed to look up workspace, treating as global"
                );
                None
            }
        }
    }
}
