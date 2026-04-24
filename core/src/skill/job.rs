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

/// Holds the result of a single upsert so we can trigger rewrites after
/// the transaction commits.
struct UpsertResult {
    original_path: String,
    canonical_file: Option<RuntimeFile>,
    needs_rewrite: bool,
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
        let mut resolved: Vec<(&SkillFileChange, Option<crate::primitives::WorkspaceId>)> =
            Vec::with_capacity(changes.files.len());
        for change in &changes.files {
            let ws_id = self.resolve_workspace_id(&change.file).await;
            resolved.push((change, ws_id));
        }

        // Batch upsert all skills in a single transaction.
        let mut op = self.skills.begin_op().await?;
        let mut results = Vec::with_capacity(resolved.len());
        for (change, ws_id) in &resolved {
            let file_hash = change.file.file_hash();
            match self
                .skills
                .upsert_from_library_in_op(&mut op, &change.file, *ws_id, file_hash)
                .await
            {
                Ok(canonical_file) => {
                    results.push(UpsertResult {
                        original_path: change.original_path.clone(),
                        canonical_file,
                        needs_rewrite: change.needs_rewrite,
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to upsert skill from library, skipping");
                }
            }
        }
        op.commit().await?;

        // Update state after the upserts committed successfully.
        // This is safe even though it's a separate transaction: if we crash
        // before persisting state, the next cycle re-processes the same files
        // and the idempotent upsert (file_hash check) skips them.
        state.last_sync_commit = Some(changes.head_commit);
        current_job.update_execution_state(&state).await?;

        // Rewrite files that lacked proper frontmatter (outside the DB
        // transaction — these are git operations).
        for result in results {
            if result.needs_rewrite {
                if let Some(canonical_file) = result.canonical_file {
                    if let Err(e) = self
                        .library
                        .rewrite_skill_file(&result.original_path, &canonical_file)
                        .await
                    {
                        tracing::warn!(
                            path = %result.original_path,
                            error = %e,
                            "failed to rewrite skill file with frontmatter"
                        );
                    }
                }
            }
        }

        Ok(())
    }

    /// Resolve workspace_id from the RuntimeFile's workspace_name field.
    async fn resolve_workspace_id(
        &self,
        file: &RuntimeFile,
    ) -> Option<crate::primitives::WorkspaceId> {
        let ws_name = match file {
            RuntimeFile::Skill { workspace_name, .. } => workspace_name.as_deref(),
            _ => None,
        };

        let ws_name = ws_name?;
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
