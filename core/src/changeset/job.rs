use std::sync::Arc;
use std::time::Duration;

use job::{
    CurrentJob, Job, JobType, ResidentJobCompletion, ResidentJobInitializer, ResidentJobRunner,
};
use serde::{Deserialize, Serialize};

use crate::github_app::GitHubAppTokenProvider;

use super::Changesets;

pub(crate) const CHANGESET_PR_POLL_JOB: &str = "changeset.pr_poll";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ChangesetPrPollConfig {
    pub interval_secs: u64,
}

impl Default for ChangesetPrPollConfig {
    fn default() -> Self {
        Self { interval_secs: 60 }
    }
}

pub(crate) struct ChangesetPrPollJobInitializer {
    changesets: Arc<Changesets>,
    github: Arc<GitHubAppTokenProvider>,
    owner: String,
    repo: String,
}

impl ChangesetPrPollJobInitializer {
    pub fn new(
        changesets: Arc<Changesets>,
        github: Arc<GitHubAppTokenProvider>,
        owner: String,
        repo: String,
    ) -> Self {
        Self {
            changesets,
            github,
            owner,
            repo,
        }
    }
}

impl ResidentJobInitializer for ChangesetPrPollJobInitializer {
    type Config = ChangesetPrPollConfig;

    fn job_type(&self) -> JobType {
        JobType::new(CHANGESET_PR_POLL_JOB)
    }

    fn init(&self, job: &Job) -> Result<Box<dyn ResidentJobRunner>, Box<dyn std::error::Error>> {
        let config: ChangesetPrPollConfig = job.config()?;
        Ok(Box::new(ChangesetPrPollRunner {
            changesets: Arc::clone(&self.changesets),
            github: Arc::clone(&self.github),
            owner: self.owner.clone(),
            repo: self.repo.clone(),
            interval: Duration::from_secs(config.interval_secs.max(1)),
        }))
    }
}

struct ChangesetPrPollRunner {
    changesets: Arc<Changesets>,
    github: Arc<GitHubAppTokenProvider>,
    owner: String,
    repo: String,
    interval: Duration,
}

#[async_trait::async_trait]
impl ResidentJobRunner for ChangesetPrPollRunner {
    #[tracing::instrument(name = "changeset.pr_poll.run", skip_all)]
    async fn run(
        &self,
        _current_job: CurrentJob,
    ) -> Result<ResidentJobCompletion, Box<dyn std::error::Error>> {
        if let Err(e) = self.poll_once().await {
            tracing::warn!(error = %e, "changeset.pr_poll: tick failed");
        }
        Ok(ResidentJobCompletion::RescheduleIn(self.interval))
    }
}

impl ChangesetPrPollRunner {
    async fn poll_once(&self) -> Result<(), Box<dyn std::error::Error>> {
        let due = self.changesets.list_submitted_with_pr().await?;
        tracing::debug!(
            count = due.len(),
            "changeset.pr_poll: reconciling submitted changesets"
        );
        for cs in due {
            let Some(pr_number) = cs.pr_number else {
                continue;
            };
            let pr = match self
                .github
                .get_pull(&self.owner, &self.repo, pr_number)
                .await
            {
                Ok(pr) => pr,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        changeset_id = %cs.id,
                        pr_number,
                        "changeset.pr_poll: get_pull failed; will retry next tick"
                    );
                    continue;
                }
            };
            if let Err(e) = self.changesets.reconcile_pr_state(cs.id, &pr).await {
                tracing::warn!(
                    error = %e,
                    changeset_id = %cs.id,
                    pr_number,
                    "changeset.pr_poll: reconcile failed; will retry next tick"
                );
            }
        }
        if let Err(e) = self.changesets.sweep_finished_refs().await {
            tracing::warn!(error = %e, "changeset.pr_poll: sweep_finished_refs failed; will retry next tick");
        }
        Ok(())
    }
}
