//! Self-rescheduling job that fires cron-trigger workflow runs.
//!
//! One job row per definition: a `WorkflowDefinitionId` is the only
//! config field. Each invocation:
//!   1. Loads the definition (no auth — system path).
//!   2. If the definition was deleted or its trigger is no longer
//!      `Cron`, exits without rescheduling.
//!   3. Otherwise spawns a run via [`Workflows::trigger_run_for_definition`]
//!      with a `triggered_by: cron` context (unless the trigger is
//!      `paused`, in which case the run is skipped), then schedules the
//!      next job at the cron expression's next fire time. A paused
//!      schedule keeps ticking so resume needs no re-registration.
//!
//! Concurrency: every cron-fired run shares the existing
//! `workflow:{definition_id}` queue used by `ExecuteRunConfig`, and the
//! cron jobs themselves share `workflow-cron:{definition_id}` so a
//! cycle can't overlap with itself if the previous spawn is slow.

use job::*;
use serde::{Deserialize, Serialize};

use crate::primitives::WorkflowDefinitionId;

use super::definition::WorkflowTrigger;
use super::repo::WorkflowDefinitionRepo;
use super::run::WorkflowRunRepo;

pub(crate) const TRIGGER_CRON_JOB: &str = "workflow.trigger-cron";

pub(crate) fn cron_queue_id(id: WorkflowDefinitionId) -> String {
    format!("workflow-cron:{id}")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerCronConfig {
    pub definition_id: WorkflowDefinitionId,
}

pub struct TriggerCronJobInitializer {
    definitions: WorkflowDefinitionRepo,
    runs: WorkflowRunRepo,
    execute_run_spawner: JobSpawner<super::job::ExecuteRunConfig>,
}

impl TriggerCronJobInitializer {
    pub(crate) fn new(
        definitions: WorkflowDefinitionRepo,
        runs: WorkflowRunRepo,
        execute_run_spawner: JobSpawner<super::job::ExecuteRunConfig>,
    ) -> Self {
        Self {
            definitions,
            runs,
            execute_run_spawner,
        }
    }
}

impl JobInitializer for TriggerCronJobInitializer {
    type Config = TriggerCronConfig;

    fn job_type(&self) -> JobType {
        JobType::new(TRIGGER_CRON_JOB)
    }

    fn init(
        &self,
        job: &Job,
        spawner: JobSpawner<Self::Config>,
    ) -> Result<Box<dyn JobRunner>, Box<dyn std::error::Error>> {
        let config: TriggerCronConfig = job.config()?;
        Ok(Box::new(TriggerCronRunner {
            definitions: self.definitions.clone(),
            runs: self.runs.clone(),
            execute_run_spawner: self.execute_run_spawner.clone(),
            spawner,
            config,
        }))
    }
}

struct TriggerCronRunner {
    definitions: WorkflowDefinitionRepo,
    runs: WorkflowRunRepo,
    execute_run_spawner: JobSpawner<super::job::ExecuteRunConfig>,
    spawner: JobSpawner<TriggerCronConfig>,
    config: TriggerCronConfig,
}

#[async_trait::async_trait]
impl JobRunner for TriggerCronRunner {
    #[tracing::instrument(
        name = "core.workflow.trigger_cron.run",
        skip_all,
        fields(definition_id = %self.config.definition_id)
    )]
    async fn run(
        &self,
        _current_job: CurrentJob,
    ) -> Result<JobCompletion, Box<dyn std::error::Error>> {
        let definition = match self
            .definitions
            .maybe_find_by_id(self.config.definition_id)
            .await?
        {
            Some(d) => d,
            None => {
                tracing::info!("definition no longer exists; cron schedule terminating");
                return Ok(JobCompletion::Complete);
            }
        };

        let (schedule, timezone, paused) = match &definition.trigger {
            WorkflowTrigger::Cron {
                schedule,
                timezone,
                paused,
                ..
            } => (schedule.clone(), timezone.clone(), *paused),
            _ => {
                tracing::info!("trigger no longer cron; schedule terminating");
                return Ok(JobCompletion::Complete);
            }
        };

        let definition_id = definition.id;
        let fired_at = chrono::Utc::now();
        let next_fire = definition.trigger.next_fire_at(fired_at);

        if paused {
            tracing::info!("cron schedule paused; skipping run, rescheduling next fire");
        } else {
            let trigger_context = serde_json::json!({
                "triggered_by": "cron",
                "fired_at": fired_at.to_rfc3339(),
                "schedule": schedule,
                "timezone": timezone,
            });

            match super::Workflows::spawn_run_static(
                &self.runs,
                &self.execute_run_spawner,
                definition,
                trigger_context,
            )
            .await
            {
                Ok(Some(run)) => {
                    tracing::info!(run_id = %run.id, "cron-triggered workflow run spawned");
                }
                Ok(None) => {
                    tracing::info!(
                        "cron-triggered run suppressed by trigger condition; schedule continues"
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, "cron-triggered run spawn failed; continuing schedule");
                }
            }
        }

        let next_at = match next_fire {
            Ok(Some(t)) => t,
            Ok(None) => {
                tracing::info!("cron expression yields no future fire; schedule terminating");
                return Ok(JobCompletion::Complete);
            }
            Err(e) => {
                tracing::warn!(error = %e, "cron evaluation failed; schedule terminating");
                return Ok(JobCompletion::Complete);
            }
        };

        let queue = cron_queue_id(definition_id);
        self.spawner
            .spawn_at_with_queue_id(JobId::new(), self.config.clone(), next_at, &queue)
            .await?;

        Ok(JobCompletion::Complete)
    }
}
