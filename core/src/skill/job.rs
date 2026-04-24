use std::time::Duration;

use job::*;
use serde::{Deserialize, Serialize};
use tokio::select;

use crate::library::{Library, RuntimeFile, SkillFileChange};
use crate::workspace::Workspaces;

use super::Skills;

pub(crate) const SYNC_SKILLS_FROM_LIBRARY_JOB: &str = "skill.sync-from-library";

const DEFAULT_SYNC_INTERVAL_SECS: u64 = 60;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SyncSkillsFromLibraryConfig {
    #[serde(default = "default_sync_interval_secs")]
    pub sync_interval_secs: u64,
}

fn default_sync_interval_secs() -> u64 {
    DEFAULT_SYNC_INTERVAL_SECS
}

pub(crate) struct SyncSkillsFromLibraryJobInitializer {
    library: Library,
    skills: Skills,
    workspaces: Workspaces,
}

impl SyncSkillsFromLibraryJobInitializer {
    pub fn new(library: Library, skills: Skills, workspaces: Workspaces) -> Self {
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

    fn retry_on_error_settings(&self) -> RetrySettings {
        RetrySettings::repeat_indefinitely()
    }

    fn init(
        &self,
        job: &Job,
        _spawner: JobSpawner<Self::Config>,
    ) -> Result<Box<dyn JobRunner>, Box<dyn std::error::Error>> {
        let config: SyncSkillsFromLibraryConfig = job.config()?;
        Ok(Box::new(SyncSkillsFromLibraryRunner {
            library: self.library.clone(),
            skills: self.skills.clone(),
            workspaces: self.workspaces.clone(),
            sync_interval: Duration::from_secs(config.sync_interval_secs),
        }))
    }
}

struct SyncSkillsFromLibraryRunner {
    library: Library,
    skills: Skills,
    workspaces: Workspaces,
    sync_interval: Duration,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SyncState {
    last_sync_commit: Option<String>,
}

#[async_trait::async_trait]
impl JobRunner for SyncSkillsFromLibraryRunner {
    #[tracing::instrument(name = "skill.sync_from_library.run", skip_all)]
    async fn run(
        &self,
        mut current_job: CurrentJob,
    ) -> Result<JobCompletion, Box<dyn std::error::Error>> {
        let mut state = current_job
            .execution_state::<SyncState>()?
            .unwrap_or_default();

        loop {
            match self.sync_once(&mut current_job, &mut state).await {
                Ok(()) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "skill sync cycle failed, will retry next interval");
                }
            }

            select! {
                biased;
                _ = current_job.shutdown_requested() => {
                    tracing::info!("shutdown requested, rescheduling sync job");
                    return Ok(JobCompletion::RescheduleNow);
                }
                _ = tokio::time::sleep(self.sync_interval) => {}
            }
        }
    }
}

impl SyncSkillsFromLibraryRunner {
    async fn sync_once(
        &self,
        current_job: &mut CurrentJob,
        state: &mut SyncState,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let changes = self
            .library
            .find_new_skills(state.last_sync_commit.as_deref())
            .await?;

        if changes.files.is_empty() {
            if !changes.head_commit.is_empty() {
                state.last_sync_commit = Some(changes.head_commit);
                current_job.update_execution_state(&state).await?;
            }
            return Ok(());
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
        // For files that need renaming (needs_rewrite), pass original_path
        // so the entity's post-persist hook propagates it through the
        // WriteToRuntime pipeline which handles the rename on disk.
        let mut op = self.skills.begin_op().await?;
        for (change, ws_id) in &resolved {
            let file_hash = change.file.file_hash();
            let original_path = if change.needs_rewrite {
                Some(change.original_path.clone())
            } else {
                None
            };
            if let Err(e) = self
                .skills
                .upsert_from_library_in_op(&mut op, &change.file, *ws_id, file_hash, original_path)
                .await
            {
                tracing::warn!(error = %e, "failed to upsert skill from library, skipping");
            }
        }
        op.commit().await?;

        // Update state after the upserts committed successfully.
        // Safe even though it's a separate transaction: if we crash before
        // persisting state, the next cycle re-processes the same files and
        // the idempotent upsert (file_hash check) skips them.
        state.last_sync_commit = Some(changes.head_commit);
        current_job.update_execution_state(&state).await?;

        Ok(())
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
