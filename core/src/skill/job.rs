use std::time::Duration;

use job::*;
use serde::{Deserialize, Serialize};
use tokio::select;

use crate::library::{Library, RuntimeFile};
use crate::workspace::Workspaces;

use super::Skills;

pub(crate) const SYNC_SKILLS_FROM_LIBRARY_JOB: &str = "skill.sync-from-library";
const SYNC_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SyncSkillsFromLibraryConfig {}

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
        _job: &Job,
        _spawner: JobSpawner<Self::Config>,
    ) -> Result<Box<dyn JobRunner>, Box<dyn std::error::Error>> {
        Ok(Box::new(SyncSkillsFromLibraryRunner {
            library: self.library.clone(),
            skills: self.skills.clone(),
            workspaces: self.workspaces.clone(),
        }))
    }
}

struct SyncSkillsFromLibraryRunner {
    library: Library,
    skills: Skills,
    workspaces: Workspaces,
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
                _ = tokio::time::sleep(SYNC_INTERVAL) => {}
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

        for file in &changes.files {
            let file_hash = file.file_hash();
            let ws_id = self.resolve_workspace_id(file).await;
            if let Err(e) = self
                .skills
                .upsert_from_library(file, ws_id, file_hash)
                .await
            {
                tracing::warn!(error = %e, "failed to upsert skill from library, skipping");
            }
        }

        state.last_sync_commit = Some(changes.head_commit);
        current_job.update_execution_state(&state).await?;
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
