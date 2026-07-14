use chrono::{DateTime, Utc};
use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;

use crate::primitives::*;
use crate::workflow::definition::WorkflowStepDef;

/// Terminal states distinguish how a run ended:
/// - `Succeeded`: every step finished and reported semantic success.
/// - `Failed`: at least one step finished cleanly but the agent
///   self-reported failure via `output.success == false`.
/// - `Errored`: at least one step hit an infrastructure-level error
///   (sandbox not ready, idle timeout, executor / agent error, etc.).
///   Errored takes precedence over Failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "VARCHAR", rename_all = "snake_case")]
pub enum WorkflowRunState {
    Pending,
    Running,
    WaitingForEvent,
    Succeeded,
    Failed,
    Errored,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepResult {
    pub name: String,
    pub output: Option<serde_json::Value>,
    pub error: Option<String>,
    pub completed_at: Option<DateTime<Utc>>,
    /// `Some(condition_body)` if the step was skipped because its
    /// `condition:` evaluated to false. Mutually exclusive with
    /// `output` and `error` — the executor records exactly one of
    /// the three terminal states. `#[serde(default)]` so older
    /// `StepResult` rows hydrate cleanly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
    /// Set when the step is a `Wait` step parked on a provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_provider: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStepState {
    Pending,
    WaitingForEvent,
    Succeeded,
    Failed,
    Errored,
    Skipped,
}

impl StepResult {
    pub fn step_state(&self) -> WorkflowStepState {
        if self.skipped.is_some() {
            WorkflowStepState::Skipped
        } else if self.error.is_some() {
            WorkflowStepState::Errored
        } else if self.step_reported_agent_failure() {
            WorkflowStepState::Failed
        } else if self.output.is_some() {
            WorkflowStepState::Succeeded
        } else if self.waiting_provider.is_some() {
            WorkflowStepState::WaitingForEvent
        } else {
            WorkflowStepState::Pending
        }
    }

    fn step_reported_agent_failure(&self) -> bool {
        if self.error.is_some() || self.skipped.is_some() {
            return false;
        }
        let reported_success = self
            .output
            .as_ref()
            .and_then(|v| v.as_object())
            .and_then(|o| o.get("success"))
            .and_then(|s| s.as_bool())
            .unwrap_or(true);
        !reported_success
    }
}

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "WorkflowRunId")]
pub enum WorkflowRunEvent {
    Initialized {
        id: WorkflowRunId,
        definition_id: WorkflowDefinitionId,
        project_id: ProjectId,
        trigger_context: serde_json::Value,
        steps_snapshot: Vec<WorkflowStepDef>,
    },
    StepStarted {
        step_name: String,
        started_at: DateTime<Utc>,
    },
    StepCompleted {
        step_name: String,
        output: serde_json::Value,
        completed_at: DateTime<Utc>,
    },
    /// Infrastructure-level error during step execution (sandbox not
    /// ready, idle timeout, agent error). Distinct from agent-reported
    /// failure (`output.success == false`), which lands as a
    /// `StepCompleted` event with a falsy `success` payload.
    StepErrored {
        step_name: String,
        error: String,
        completed_at: DateTime<Utc>,
    },
    /// The step's `condition:` evaluated to `false`. The run
    /// continues to the next step. Folds into run-state aggregation
    /// as Succeeded (skipped steps are invisible to the
    /// Errored/Failed/Succeeded classification).
    StepSkipped {
        step_name: String,
        /// Raw CEL body that was false at evaluation time. Kept on
        /// the event for forensic visibility in `runs --include=steps`.
        condition_body: String,
        completed_at: DateTime<Utc>,
    },
    StepWaiting {
        step_name: String,
        provider: String,
        started_at: DateTime<Utc>,
    },
    StepResumed {
        step_name: String,
        output: serde_json::Value,
        source: String,
        resumed_at: DateTime<Utc>,
    },
    /// Emitted at the start of each iteration of a `Loop` step.
    /// `iteration` is 1-based. Inner steps in this iteration record
    /// themselves under dotted names like
    /// `{step_name}.iter[{iteration}].{inner_name}`.
    LoopIterationStarted {
        step_name: String,
        iteration: u32,
        started_at: DateTime<Utc>,
    },
    /// Emitted when an iteration's last inner step finishes, before
    /// the next iteration starts (or before the outer loop's
    /// `StepCompleted` if `broke` is true / `max_iterations` exhausted).
    LoopIterationCompleted {
        step_name: String,
        iteration: u32,
        /// `true` → this iteration's `break_condition` evaluated to
        /// true; the loop terminates here. `false` → the loop will
        /// move to iteration+1 (or terminate via max-iterations
        /// exhaustion, recorded on the outer step's `StepCompleted`).
        broke: bool,
        completed_at: DateTime<Utc>,
    },
    RunCompleted {
        state: WorkflowRunState,
        completed_at: DateTime<Utc>,
    },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct WorkflowRun {
    pub id: WorkflowRunId,
    pub definition_id: WorkflowDefinitionId,
    pub project_id: ProjectId,
    pub trigger_context: serde_json::Value,
    pub steps_snapshot: Vec<WorkflowStepDef>,
    #[builder(default = "WorkflowRunState::Pending")]
    pub state: WorkflowRunState,
    #[builder(default)]
    pub completed_at: Option<DateTime<Utc>>,
    #[builder(default)]
    pub step_results: Vec<StepResult>,
    events: EntityEvents<WorkflowRunEvent>,
}

impl WorkflowRun {
    pub fn started_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.events
            .entity_first_persisted_at()
            .expect("entity_first_persisted_at not found")
    }

    fn any_step_errored(&self) -> bool {
        self.step_results.iter().any(|r| r.error.is_some())
    }

    fn any_step_reported_failure(&self) -> bool {
        self.step_results
            .iter()
            .any(StepResult::step_reported_agent_failure)
    }

    pub fn step_already_terminal(&self, step_name: &str) -> bool {
        self.step_results
            .iter()
            .any(|r| r.name == step_name && r.completed_at.is_some())
    }

    /// No-op if any prior event for this step is already recorded —
    /// keeps at-least-once job retries safe.
    pub fn step_started(&mut self, step_name: String) -> Idempotent<()> {
        idempotency_guard!(
            self.events.iter_all().rev(),
            already_applied:
                WorkflowRunEvent::StepStarted { step_name: n, .. } if n == &step_name,
            already_applied:
                WorkflowRunEvent::StepCompleted { step_name: n, .. } if n == &step_name,
            already_applied:
                WorkflowRunEvent::StepErrored { step_name: n, .. } if n == &step_name,
            already_applied:
                WorkflowRunEvent::StepSkipped { step_name: n, .. } if n == &step_name,
        );
        let now = Utc::now();
        if !self.step_results.iter().any(|r| r.name == step_name) {
            self.step_results.push(StepResult {
                name: step_name.clone(),
                output: None,
                error: None,
                completed_at: None,
                skipped: None,
                waiting_provider: None,
            });
        }
        if self.state == WorkflowRunState::Pending {
            self.state = WorkflowRunState::Running;
        }
        self.events.push(WorkflowRunEvent::StepStarted {
            step_name,
            started_at: now,
        });
        Idempotent::Executed(())
    }

    /// No-op if the step already terminated.
    pub fn step_completed(
        &mut self,
        step_name: String,
        output: serde_json::Value,
    ) -> Idempotent<()> {
        idempotency_guard!(
            self.events.iter_all().rev(),
            already_applied:
                WorkflowRunEvent::StepCompleted { step_name: n, .. } if n == &step_name,
            already_applied:
                WorkflowRunEvent::StepErrored { step_name: n, .. } if n == &step_name,
            already_applied:
                WorkflowRunEvent::StepSkipped { step_name: n, .. } if n == &step_name,
        );
        let now = Utc::now();
        if let Some(r) = self.step_results.iter_mut().find(|r| r.name == step_name) {
            r.output = Some(output.clone());
            r.completed_at = Some(now);
        } else {
            self.step_results.push(StepResult {
                name: step_name.clone(),
                output: Some(output.clone()),
                error: None,
                completed_at: Some(now),
                skipped: None,
                waiting_provider: None,
            });
        }
        self.events.push(WorkflowRunEvent::StepCompleted {
            step_name,
            output,
            completed_at: now,
        });
        Idempotent::Executed(())
    }

    /// Records an infrastructure-level step error (sandbox not ready,
    /// idle timeout, agent error). Agent-reported failure
    /// (`output.success == false`) goes through `step_completed`, not
    /// here — those are aggregated into `WorkflowRunState::Failed`,
    /// while errors aggregated here become `WorkflowRunState::Errored`.
    ///
    /// Drives the same Pending → Running transition that
    /// `step_started` and `step_skipped` do — the executor's
    /// condition-gate error paths call this directly without a
    /// prior `step_started`, so without the transition a run whose
    /// very first step's condition errored would jump from Pending
    /// straight to Errored, leaving no `Running` phase in the
    /// event log.
    ///
    /// No-op if the step already terminated.
    pub fn step_errored(&mut self, step_name: String, error: String) -> Idempotent<()> {
        idempotency_guard!(
            self.events.iter_all().rev(),
            already_applied:
                WorkflowRunEvent::StepErrored { step_name: n, .. } if n == &step_name,
            already_applied:
                WorkflowRunEvent::StepCompleted { step_name: n, .. } if n == &step_name,
            already_applied:
                WorkflowRunEvent::StepSkipped { step_name: n, .. } if n == &step_name,
        );
        let now = Utc::now();
        if let Some(r) = self.step_results.iter_mut().find(|r| r.name == step_name) {
            r.error = Some(error.clone());
            r.completed_at = Some(now);
        } else {
            self.step_results.push(StepResult {
                name: step_name.clone(),
                output: None,
                error: Some(error.clone()),
                completed_at: Some(now),
                skipped: None,
                waiting_provider: None,
            });
        }
        if self.state == WorkflowRunState::Pending {
            self.state = WorkflowRunState::Running;
        }
        self.events.push(WorkflowRunEvent::StepErrored {
            step_name,
            error,
            completed_at: now,
        });
        Idempotent::Executed(())
    }

    /// Records that the step was skipped because its `condition:`
    /// evaluated to false. The run continues to the next step.
    /// `condition_body` is the raw CEL expression — kept on the
    /// event so `runs --include=steps` can render which gate fired.
    ///
    /// The executor calls this directly (without a prior
    /// `step_started`) when the gate evaluates false, so this method
    /// must drive the same Pending → Running state transition that
    /// `step_started` does — otherwise a run with every step gated
    /// out would jump from Pending straight to Succeeded at
    /// `run_completed`, with no `Running` phase in its event log.
    ///
    /// No-op if the step already terminated.
    pub fn step_skipped(&mut self, step_name: String, condition_body: String) -> Idempotent<()> {
        idempotency_guard!(
            self.events.iter_all().rev(),
            already_applied:
                WorkflowRunEvent::StepSkipped { step_name: n, .. } if n == &step_name,
            already_applied:
                WorkflowRunEvent::StepCompleted { step_name: n, .. } if n == &step_name,
            already_applied:
                WorkflowRunEvent::StepErrored { step_name: n, .. } if n == &step_name,
        );
        let now = Utc::now();
        if let Some(r) = self.step_results.iter_mut().find(|r| r.name == step_name) {
            r.skipped = Some(condition_body.clone());
            r.completed_at = Some(now);
        } else {
            self.step_results.push(StepResult {
                name: step_name.clone(),
                output: None,
                error: None,
                completed_at: Some(now),
                skipped: Some(condition_body.clone()),
                waiting_provider: None,
            });
        }
        if self.state == WorkflowRunState::Pending {
            self.state = WorkflowRunState::Running;
        }
        self.events.push(WorkflowRunEvent::StepSkipped {
            step_name,
            condition_body,
            completed_at: now,
        });
        Idempotent::Executed(())
    }

    /// Finalises the run, classifying its terminal state from the
    /// recorded step results:
    /// - any errored step → `Errored`
    /// - else any agent-reported failure (`output.success == false`) → `Failed`
    /// - else → `Succeeded`
    ///
    /// No-op if the run already reached a terminal state or is
    /// parked on a wait step.
    pub fn run_completed(&mut self) -> Idempotent<()> {
        idempotency_guard!(
            self.events.iter_all().rev(),
            already_applied: WorkflowRunEvent::RunCompleted { .. },
        );
        if self.state == WorkflowRunState::WaitingForEvent {
            return Idempotent::AlreadyApplied;
        }
        let state = if self.any_step_errored() {
            WorkflowRunState::Errored
        } else if self.any_step_reported_failure() {
            WorkflowRunState::Failed
        } else {
            WorkflowRunState::Succeeded
        };
        let now = Utc::now();
        self.state = state;
        self.completed_at = Some(now);
        self.events.push(WorkflowRunEvent::RunCompleted {
            state,
            completed_at: now,
        });
        Idempotent::Executed(())
    }

    /// Records the start of a `Loop` step's iteration. Driven by the
    /// executor before it begins running that iteration's inner
    /// steps. Idempotent: replaying the same `(step_name, iteration)`
    /// pair is a no-op so at-least-once job retries are safe.
    pub fn loop_iteration_started(&mut self, step_name: String, iteration: u32) -> Idempotent<()> {
        idempotency_guard!(
            self.events.iter_all().rev(),
            already_applied:
                WorkflowRunEvent::LoopIterationStarted { step_name: n, iteration: i, .. }
                    if n == &step_name && *i == iteration,
        );
        let now = Utc::now();
        if self.state == WorkflowRunState::Pending {
            self.state = WorkflowRunState::Running;
        }
        self.events.push(WorkflowRunEvent::LoopIterationStarted {
            step_name,
            iteration,
            started_at: now,
        });
        Idempotent::Executed(())
    }

    /// Records the end of a `Loop` step's iteration after its
    /// `break_condition` has been evaluated. `broke == true` →
    /// the executor exits the loop here. Idempotent on
    /// `(step_name, iteration)`.
    pub fn loop_iteration_completed(
        &mut self,
        step_name: String,
        iteration: u32,
        broke: bool,
    ) -> Idempotent<()> {
        idempotency_guard!(
            self.events.iter_all().rev(),
            already_applied:
                WorkflowRunEvent::LoopIterationCompleted { step_name: n, iteration: i, .. }
                    if n == &step_name && *i == iteration,
        );
        let now = Utc::now();
        self.events.push(WorkflowRunEvent::LoopIterationCompleted {
            step_name,
            iteration,
            broke,
            completed_at: now,
        });
        Idempotent::Executed(())
    }

    /// Highest iteration of a Loop step for which `LoopIterationStarted`
    /// has been emitted. `0` → no iteration has begun yet. Used by the
    /// executor on resume to pick up where the previous attempt left off.
    pub fn loop_current_iteration(&self, step_name: &str) -> u32 {
        let mut max = 0u32;
        for ev in self.events.iter_all() {
            if let WorkflowRunEvent::LoopIterationStarted {
                step_name: n,
                iteration,
                ..
            } = ev
            {
                if n == step_name && *iteration > max {
                    max = *iteration;
                }
            }
        }
        max
    }

    /// True once `LoopIterationCompleted { broke: true }` has been
    /// emitted for this loop step OR the outer step itself has
    /// reached a terminal state. The executor uses this to short-
    /// circuit the inner iteration loop on resume.
    pub fn loop_broke(&self, step_name: &str) -> bool {
        for ev in self.events.iter_all() {
            if let WorkflowRunEvent::LoopIterationCompleted {
                step_name: n,
                broke,
                ..
            } = ev
            {
                if n == step_name && *broke {
                    return true;
                }
            }
        }
        false
    }

    /// All `LoopIterationCompleted` outputs collected from the
    /// recorded inner step results. Used to populate the `iterations`
    /// CEL binding and to compute the outer loop's `StepResult.output`.
    pub fn loop_iterations_so_far(&self, step_name: &str) -> Vec<serde_json::Value> {
        let mut by_iter: std::collections::BTreeMap<
            u32,
            serde_json::Map<String, serde_json::Value>,
        > = std::collections::BTreeMap::new();
        let prefix = format!("{step_name}.iter[");
        for r in &self.step_results {
            if !r.name.starts_with(&prefix) {
                continue;
            }
            // `{step}.iter[{N}].{inner}` — extract N and inner
            let after = &r.name[prefix.len()..];
            let Some(close_idx) = after.find(']') else {
                continue;
            };
            let Ok(iteration) = after[..close_idx].parse::<u32>() else {
                continue;
            };
            // Skip the leading "]." then take the remainder
            let inner = match after.get(close_idx + 2..) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let inner_output = r.output.clone().unwrap_or(serde_json::Value::Null);
            by_iter
                .entry(iteration)
                .or_default()
                .insert(inner, inner_output);
        }
        let broke_map: std::collections::HashMap<u32, bool> = self
            .events
            .iter_all()
            .filter_map(|ev| match ev {
                WorkflowRunEvent::LoopIterationCompleted {
                    step_name: n,
                    iteration,
                    broke,
                    ..
                } if n == step_name => Some((*iteration, *broke)),
                _ => None,
            })
            .collect();
        by_iter
            .into_iter()
            .map(|(iteration, steps)| {
                let broke = broke_map.get(&iteration).copied().unwrap_or(false);
                serde_json::json!({
                    "iteration": iteration,
                    "steps": serde_json::Value::Object(steps),
                    "broke": broke,
                })
            })
            .collect()
    }

    /// Transitions Running → WaitingForEvent when the executor
    /// reaches a Wait step.
    pub fn step_waiting(&mut self, step_name: String, provider: String) -> Idempotent<()> {
        idempotency_guard!(
            self.events.iter_all().rev(),
            already_applied:
                WorkflowRunEvent::StepWaiting { step_name: n, .. } if n == &step_name,
            already_applied:
                WorkflowRunEvent::StepResumed { step_name: n, .. } if n == &step_name,
            already_applied:
                WorkflowRunEvent::StepCompleted { step_name: n, .. } if n == &step_name,
            already_applied:
                WorkflowRunEvent::StepErrored { step_name: n, .. } if n == &step_name,
            already_applied:
                WorkflowRunEvent::StepSkipped { step_name: n, .. } if n == &step_name,
        );
        let now = Utc::now();
        if let Some(r) = self.step_results.iter_mut().find(|r| r.name == step_name) {
            r.waiting_provider = Some(provider.clone());
        } else {
            self.step_results.push(StepResult {
                name: step_name.clone(),
                output: None,
                error: None,
                completed_at: None,
                skipped: None,
                waiting_provider: Some(provider.clone()),
            });
        }
        self.state = WorkflowRunState::WaitingForEvent;
        self.events.push(WorkflowRunEvent::StepWaiting {
            step_name,
            provider,
            started_at: now,
        });
        Idempotent::Executed(())
    }

    /// Transitions WaitingForEvent → Running when an inbound event
    /// matched the wait step's resume_condition.
    pub fn step_resumed(
        &mut self,
        step_name: String,
        output: serde_json::Value,
        source: String,
    ) -> Idempotent<()> {
        idempotency_guard!(
            self.events.iter_all().rev(),
            already_applied:
                WorkflowRunEvent::StepResumed { step_name: n, .. } if n == &step_name,
            already_applied:
                WorkflowRunEvent::StepCompleted { step_name: n, .. } if n == &step_name,
            already_applied:
                WorkflowRunEvent::StepErrored { step_name: n, .. } if n == &step_name,
        );
        let now = Utc::now();
        if let Some(r) = self.step_results.iter_mut().find(|r| r.name == step_name) {
            r.output = Some(output.clone());
            r.completed_at = Some(now);
            r.waiting_provider = None;
        } else {
            self.step_results.push(StepResult {
                name: step_name.clone(),
                output: Some(output.clone()),
                error: None,
                completed_at: Some(now),
                skipped: None,
                waiting_provider: None,
            });
        }
        self.state = WorkflowRunState::Running;
        self.events.push(WorkflowRunEvent::StepResumed {
            step_name,
            output,
            source,
            resumed_at: now,
        });
        Idempotent::Executed(())
    }

    /// Returns the step that is currently in WaitingForEvent state.
    pub fn current_wait_step(&self) -> Option<&StepResult> {
        if self.state != WorkflowRunState::WaitingForEvent {
            return None;
        }
        self.step_results.iter().find(|r| {
            r.waiting_provider.is_some()
                && r.output.is_none()
                && r.error.is_none()
                && r.skipped.is_none()
                && r.completed_at.is_none()
        })
    }
}

impl core::fmt::Display for WorkflowRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "WorkflowRun: {}, definition: {}, state: {:?}",
            self.id, self.definition_id, self.state
        )
    }
}

impl TryFromEvents<WorkflowRunEvent> for WorkflowRun {
    fn try_from_events(
        events: EntityEvents<WorkflowRunEvent>,
    ) -> Result<Self, EntityHydrationError> {
        let mut builder = WorkflowRunBuilder::default();
        let mut state = WorkflowRunState::Pending;
        let mut completed_at: Option<DateTime<Utc>> = None;
        let mut results: Vec<StepResult> = Vec::new();

        for event in events.iter_all() {
            match event {
                WorkflowRunEvent::Initialized {
                    id,
                    definition_id,
                    project_id,
                    trigger_context,
                    steps_snapshot,
                } => {
                    builder = builder
                        .id(*id)
                        .definition_id(*definition_id)
                        .project_id(*project_id)
                        .trigger_context(trigger_context.clone())
                        .steps_snapshot(steps_snapshot.clone());
                }
                WorkflowRunEvent::StepStarted { step_name, .. } => {
                    state = WorkflowRunState::Running;
                    if !results.iter().any(|r| &r.name == step_name) {
                        results.push(StepResult {
                            name: step_name.clone(),
                            output: None,
                            error: None,
                            completed_at: None,
                            skipped: None,
                            waiting_provider: None,
                        });
                    }
                }
                WorkflowRunEvent::StepCompleted {
                    step_name,
                    output,
                    completed_at: ts,
                } => {
                    if let Some(r) = results.iter_mut().find(|r| &r.name == step_name) {
                        r.output = Some(output.clone());
                        r.completed_at = Some(*ts);
                    } else {
                        results.push(StepResult {
                            name: step_name.clone(),
                            output: Some(output.clone()),
                            error: None,
                            completed_at: Some(*ts),
                            skipped: None,
                            waiting_provider: None,
                        });
                    }
                }
                WorkflowRunEvent::StepErrored {
                    step_name,
                    error,
                    completed_at: ts,
                } => {
                    state = WorkflowRunState::Running;
                    if let Some(r) = results.iter_mut().find(|r| &r.name == step_name) {
                        r.error = Some(error.clone());
                        r.completed_at = Some(*ts);
                    } else {
                        results.push(StepResult {
                            name: step_name.clone(),
                            output: None,
                            error: Some(error.clone()),
                            completed_at: Some(*ts),
                            skipped: None,
                            waiting_provider: None,
                        });
                    }
                }
                WorkflowRunEvent::StepSkipped {
                    step_name,
                    condition_body,
                    completed_at: ts,
                } => {
                    state = WorkflowRunState::Running;
                    if let Some(r) = results.iter_mut().find(|r| &r.name == step_name) {
                        r.skipped = Some(condition_body.clone());
                        r.completed_at = Some(*ts);
                    } else {
                        results.push(StepResult {
                            name: step_name.clone(),
                            output: None,
                            error: None,
                            completed_at: Some(*ts),
                            skipped: Some(condition_body.clone()),
                            waiting_provider: None,
                        });
                    }
                }
                WorkflowRunEvent::LoopIterationStarted { .. } => {
                    state = WorkflowRunState::Running;
                }
                WorkflowRunEvent::LoopIterationCompleted { .. } => {
                    state = WorkflowRunState::Running;
                }
                WorkflowRunEvent::StepWaiting {
                    step_name,
                    provider,
                    ..
                } => {
                    state = WorkflowRunState::WaitingForEvent;
                    if let Some(r) = results.iter_mut().find(|r| &r.name == step_name) {
                        r.waiting_provider = Some(provider.clone());
                    } else {
                        results.push(StepResult {
                            name: step_name.clone(),
                            output: None,
                            error: None,
                            completed_at: None,
                            skipped: None,
                            waiting_provider: Some(provider.clone()),
                        });
                    }
                }
                WorkflowRunEvent::StepResumed {
                    step_name,
                    output,
                    resumed_at,
                    ..
                } => {
                    state = WorkflowRunState::Running;
                    if let Some(r) = results.iter_mut().find(|r| &r.name == step_name) {
                        r.output = Some(output.clone());
                        r.completed_at = Some(*resumed_at);
                        r.waiting_provider = None;
                    } else {
                        results.push(StepResult {
                            name: step_name.clone(),
                            output: Some(output.clone()),
                            error: None,
                            completed_at: Some(*resumed_at),
                            skipped: None,
                            waiting_provider: None,
                        });
                    }
                }
                WorkflowRunEvent::RunCompleted {
                    state: s,
                    completed_at: ts,
                } => {
                    state = *s;
                    completed_at = Some(*ts);
                }
            }
        }

        builder = builder.state(state).step_results(results);
        builder = builder.completed_at(completed_at);

        builder.events(events).build()
    }
}

#[derive(Debug, Builder)]
#[builder(pattern = "owned")]
pub struct NewWorkflowRun {
    #[builder(setter(into))]
    pub(crate) id: WorkflowRunId,
    #[builder(setter(into))]
    pub(crate) definition_id: WorkflowDefinitionId,
    #[builder(setter(into))]
    pub(crate) project_id: ProjectId,
    pub(crate) trigger_context: serde_json::Value,
    pub(crate) steps_snapshot: Vec<WorkflowStepDef>,
}

impl NewWorkflowRun {
    pub fn builder() -> NewWorkflowRunBuilder {
        NewWorkflowRunBuilder::default().id(WorkflowRunId::new())
    }

    pub(crate) fn initial_state(&self) -> WorkflowRunState {
        WorkflowRunState::Pending
    }
}

impl IntoEvents<WorkflowRunEvent> for NewWorkflowRun {
    fn into_events(self) -> EntityEvents<WorkflowRunEvent> {
        EntityEvents::init(
            self.id,
            [WorkflowRunEvent::Initialized {
                id: self.id,
                definition_id: self.definition_id,
                project_id: self.project_id,
                trigger_context: self.trigger_context,
                steps_snapshot: self.steps_snapshot,
            }],
        )
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::super::definition::default_output_schema;
    use super::*;

    fn sample_step(name: &str) -> WorkflowStepDef {
        WorkflowStepDef::AgentStep {
            name: name.to_string(),
            skill: "echo-test".to_string(),
            sandbox: None,
            sandbox_mode: None,
            timeout_seconds: None,
            model_chain: None,
            output_schema: Box::new(default_output_schema()),
            condition: None,
        }
    }

    fn fresh_run(step_names: &[&str]) -> WorkflowRun {
        let new = NewWorkflowRun::builder()
            .definition_id(WorkflowDefinitionId::new())
            .project_id(ProjectId::new())
            .trigger_context(json!({}))
            .steps_snapshot(step_names.iter().map(|n| sample_step(n)).collect())
            .build()
            .unwrap();
        WorkflowRun::try_from_events(new.into_events()).unwrap()
    }

    fn start(run: &mut WorkflowRun, name: &str) {
        run.step_started(name.into()).did_execute();
    }

    fn complete(run: &mut WorkflowRun, name: &str, output: serde_json::Value) {
        run.step_completed(name.into(), output).did_execute();
    }

    fn error(run: &mut WorkflowRun, name: &str, err: &str) {
        run.step_errored(name.into(), err.into()).did_execute();
    }

    fn skip(run: &mut WorkflowRun, name: &str, body: &str) {
        run.step_skipped(name.into(), body.into()).did_execute();
    }

    fn finalize(run: &mut WorkflowRun) -> WorkflowRunState {
        run.run_completed().did_execute();
        run.state
    }

    #[test]
    fn run_state_succeeded_when_all_steps_completed_with_success_true() {
        let mut run = fresh_run(&["only"]);
        start(&mut run, "only");
        complete(
            &mut run,
            "only",
            json!({ "success": true, "reason": "ok", "output": "did the thing" }),
        );

        assert_eq!(finalize(&mut run), WorkflowRunState::Succeeded);
    }

    #[test]
    fn run_state_failed_when_step_completed_with_success_false() {
        let mut run = fresh_run(&["only"]);
        start(&mut run, "only");
        complete(
            &mut run,
            "only",
            json!({
                "success": false,
                "reason": "gave up: cargo fmt unavailable",
                "output": "gave-up | already_formatted | build #592",
            }),
        );

        assert_eq!(finalize(&mut run), WorkflowRunState::Failed);
    }

    #[test]
    fn run_state_succeeded_when_step_has_no_success_field() {
        let mut run = fresh_run(&["only"]);
        start(&mut run, "only");
        complete(
            &mut run,
            "only",
            json!({ "verdict": "pass", "notes": "looks good" }),
        );

        assert_eq!(finalize(&mut run), WorkflowRunState::Succeeded);
    }

    #[test]
    fn run_state_succeeded_when_step_output_is_non_object() {
        let mut run = fresh_run(&["only"]);
        start(&mut run, "only");
        complete(&mut run, "only", json!("free-text completion"));

        assert_eq!(finalize(&mut run), WorkflowRunState::Succeeded);
    }

    #[test]
    fn run_state_errored_when_step_hits_infrastructure_error() {
        let mut run = fresh_run(&["only"]);
        start(&mut run, "only");
        error(&mut run, "only", "sandbox not ready");

        assert_eq!(finalize(&mut run), WorkflowRunState::Errored);
    }

    /// The executor's condition-gate error paths
    /// (`ConditionOutcome::NotBoolean`, CEL runtime error, parse
    /// error) call `step_errored` directly without a prior
    /// `step_started`. Same wart as `step_skipped` had — without
    /// the Pending → Running transition on `step_errored`, a run
    /// whose very first step's condition errored would jump from
    /// Pending straight to Errored at `run_completed`, with no
    /// `Running` phase ever recorded.
    #[test]
    fn step_errored_transitions_pending_to_running() {
        let mut run = fresh_run(&["only"]);
        assert_eq!(run.state, WorkflowRunState::Pending);
        error(&mut run, "only", "condition body returned non-boolean");
        assert_eq!(run.state, WorkflowRunState::Running);
    }

    /// Production condition-gate error path (`else` branch of
    /// `step_errored`): no prior `step_started`, fresh `StepResult`
    /// pushed with the error message.
    #[test]
    fn step_errored_creates_fresh_step_result() {
        let mut run = fresh_run(&["only"]);
        error(&mut run, "only", "condition body returned non-boolean");
        assert_eq!(run.step_results.len(), 1);
        let r = &run.step_results[0];
        assert_eq!(r.name, "only");
        assert_eq!(
            r.error.as_deref(),
            Some("condition body returned non-boolean")
        );
        assert!(r.output.is_none());
        assert!(r.skipped.is_none());
        assert!(r.completed_at.is_some());
    }

    /// Mid-flight rehydration: a run whose only event is
    /// `StepErrored` (no prior `StepStarted`) must reflect Running
    /// state, mirroring the live mutation. Same fix as the
    /// `StepSkipped` arm.
    #[test]
    fn step_errored_hydrates_with_running_state() {
        let mut run = fresh_run(&["only"]);
        error(&mut run, "only", "condition body returned non-boolean");
        let events = run.events;
        let rehydrated = WorkflowRun::try_from_events(events).unwrap();
        assert_eq!(rehydrated.state, WorkflowRunState::Running);
        assert_eq!(rehydrated.step_results.len(), 1);
        assert_eq!(
            rehydrated.step_results[0].error.as_deref(),
            Some("condition body returned non-boolean")
        );
    }

    #[test]
    fn errored_takes_precedence_over_agent_reported_failure() {
        let mut run = fresh_run(&["a", "b"]);
        start(&mut run, "a");
        complete(
            &mut run,
            "a",
            json!({ "success": false, "reason": "agent gave up", "output": "" }),
        );
        start(&mut run, "b");
        error(&mut run, "b", "idle timeout");

        assert_eq!(finalize(&mut run), WorkflowRunState::Errored);
    }

    #[test]
    fn run_state_failed_when_first_step_succeeds_but_second_returns_success_false() {
        let mut run = fresh_run(&["a", "b"]);
        start(&mut run, "a");
        complete(
            &mut run,
            "a",
            json!({ "success": true, "reason": "", "output": "done" }),
        );
        start(&mut run, "b");
        complete(
            &mut run,
            "b",
            json!({ "success": false, "reason": "no", "output": "gave-up" }),
        );

        assert_eq!(finalize(&mut run), WorkflowRunState::Failed);
    }

    #[test]
    fn step_errored_event_wire_format() {
        let ev = WorkflowRunEvent::StepErrored {
            step_name: "s".into(),
            error: "boom".into(),
            completed_at: Utc::now(),
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v.get("type").and_then(|t| t.as_str()), Some("step_errored"));
    }

    #[test]
    fn run_state_handles_non_bool_success_field_as_succeeded() {
        let mut run = fresh_run(&["only"]);
        start(&mut run, "only");
        complete(
            &mut run,
            "only",
            json!({ "success": "yes", "reason": "string-typed" }),
        );

        assert_eq!(finalize(&mut run), WorkflowRunState::Succeeded);
    }

    #[test]
    fn step_skipped_event_wire_format() {
        let ev = WorkflowRunEvent::StepSkipped {
            step_name: "s".into(),
            condition_body: "trigger.payload.x == 'y'".into(),
            completed_at: Utc::now(),
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v.get("type").and_then(|t| t.as_str()), Some("step_skipped"));
        assert_eq!(
            v.get("condition_body").and_then(|c| c.as_str()),
            Some("trigger.payload.x == 'y'")
        );
    }

    /// Production calls `step_skipped` directly (no prior
    /// `step_started`) when the condition gate evaluates false —
    /// the executor's "skipped steps never enter Running state"
    /// invariant. This test mirrors that path: the `else` branch in
    /// `step_skipped` (push fresh `StepResult`) is the production
    /// path; the `if let Some` branch only fires on hydration replay.
    #[test]
    fn run_state_succeeded_when_only_step_was_skipped() {
        let mut run = fresh_run(&["only"]);
        skip(&mut run, "only", "trigger.payload.flag == true");
        assert_eq!(finalize(&mut run), WorkflowRunState::Succeeded);
    }

    /// `step_skipped` mirrors `step_started`'s Pending → Running
    /// transition. Without it, an all-skipped run would stay
    /// Pending throughout the executor loop and only flip to a
    /// terminal state at `run_completed` — leaving no `Running`
    /// phase in the event log. This test pins the transition.
    #[test]
    fn step_skipped_transitions_pending_to_running() {
        let mut run = fresh_run(&["only"]);
        assert_eq!(run.state, WorkflowRunState::Pending);
        skip(&mut run, "only", "trigger.payload.flag == true");
        assert_eq!(run.state, WorkflowRunState::Running);
    }

    /// The production path creates a fresh `StepResult` (the `else`
    /// branch of `step_skipped`) since the executor never called
    /// `step_started` for a gated-out step. Verify the new entry
    /// is well-formed: condition body recorded, no output, no
    /// error, `completed_at` set.
    #[test]
    fn step_skipped_creates_fresh_step_result() {
        let mut run = fresh_run(&["only"]);
        skip(&mut run, "only", "trigger.payload.flag == true");
        assert_eq!(run.step_results.len(), 1);
        let r = &run.step_results[0];
        assert_eq!(r.name, "only");
        assert_eq!(r.skipped.as_deref(), Some("trigger.payload.flag == true"));
        assert!(r.output.is_none());
        assert!(r.error.is_none());
        assert!(r.completed_at.is_some());
    }

    #[test]
    fn run_state_succeeded_when_skip_mixes_with_success() {
        let mut run = fresh_run(&["a", "b"]);
        skip(&mut run, "a", "trigger.payload.flag == true");
        start(&mut run, "b");
        complete(
            &mut run,
            "b",
            json!({ "success": true, "output": "did the thing" }),
        );
        assert_eq!(finalize(&mut run), WorkflowRunState::Succeeded);
    }

    #[test]
    fn run_state_failed_when_skip_mixes_with_agent_failure() {
        let mut run = fresh_run(&["a", "b"]);
        skip(&mut run, "a", "trigger.payload.flag == true");
        start(&mut run, "b");
        complete(
            &mut run,
            "b",
            json!({ "success": false, "reason": "no", "output": "" }),
        );
        assert_eq!(finalize(&mut run), WorkflowRunState::Failed);
    }

    #[test]
    fn run_state_errored_when_skip_mixes_with_infra_error() {
        let mut run = fresh_run(&["a", "b"]);
        skip(&mut run, "a", "trigger.payload.flag == true");
        start(&mut run, "b");
        error(&mut run, "b", "sandbox not ready");
        assert_eq!(finalize(&mut run), WorkflowRunState::Errored);
    }

    #[test]
    fn step_skipped_is_idempotent_against_retry() {
        let mut run = fresh_run(&["only"]);
        skip(&mut run, "only", "x == 'y'");
        // Replay the same event — should be a no-op.
        let outcome = run.step_skipped("only".into(), "x == 'y'".into());
        assert!(matches!(outcome, Idempotent::AlreadyApplied));
    }

    #[test]
    fn step_skipped_blocks_subsequent_completed_or_errored() {
        let mut run = fresh_run(&["only"]);
        skip(&mut run, "only", "x == 'y'");
        // Subsequent terminal mutations are no-ops.
        let outcome = run.step_completed("only".into(), json!({ "success": true }));
        assert!(matches!(outcome, Idempotent::AlreadyApplied));
        let outcome = run.step_errored("only".into(), "boom".into());
        assert!(matches!(outcome, Idempotent::AlreadyApplied));
    }

    #[test]
    fn step_skipped_hydrates_from_events() {
        let mut run = fresh_run(&["only"]);
        skip(&mut run, "only", "trigger.payload.x == 'y'");
        let events = run.events;
        let rehydrated = WorkflowRun::try_from_events(events).unwrap();
        assert_eq!(rehydrated.step_results.len(), 1);
        assert_eq!(
            rehydrated.step_results[0].skipped.as_deref(),
            Some("trigger.payload.x == 'y'")
        );
        assert!(rehydrated.step_results[0].output.is_none());
        assert!(rehydrated.step_results[0].error.is_none());
        assert!(rehydrated.step_results[0].completed_at.is_some());
        // Pending → Running transition replays from the StepSkipped
        // event in the same way StepStarted drives it, so a run
        // hydrated mid-flight reflects the live state.
        assert_eq!(rehydrated.state, WorkflowRunState::Running);
    }

    // ── Wait step tests ──

    #[test]
    fn step_waiting_transitions_running_to_waiting_for_event() {
        let mut run = fresh_run(&["a", "wait_step"]);
        start(&mut run, "a");
        complete(&mut run, "a", json!({ "success": true }));
        assert_eq!(run.state, WorkflowRunState::Running);

        run.step_waiting("wait_step".into(), "github_app".into())
            .did_execute();
        assert_eq!(run.state, WorkflowRunState::WaitingForEvent);
        assert_eq!(run.step_results.len(), 2);
        assert_eq!(
            run.step_results[1].waiting_provider.as_deref(),
            Some("github_app")
        );
        assert!(run.step_results[1].output.is_none());
        assert!(run.step_results[1].completed_at.is_none());
    }

    #[test]
    fn step_waiting_transitions_pending_to_waiting_for_event() {
        let mut run = fresh_run(&["wait_step"]);
        run.step_waiting("wait_step".into(), "github_app".into())
            .did_execute();
        assert_eq!(run.state, WorkflowRunState::WaitingForEvent);
    }

    #[test]
    fn step_resumed_transitions_waiting_to_running() {
        let mut run = fresh_run(&["wait_step", "after"]);
        run.step_waiting("wait_step".into(), "github_app".into())
            .did_execute();
        assert_eq!(run.state, WorkflowRunState::WaitingForEvent);

        let output = json!({ "reviewer": "alice", "action": "approved" });
        run.step_resumed(
            "wait_step".into(),
            output.clone(),
            "provider:github_app".into(),
        )
        .did_execute();
        assert_eq!(run.state, WorkflowRunState::Running);
        let sr = &run.step_results[0];
        assert_eq!(sr.output.as_ref(), Some(&output));
        assert!(sr.completed_at.is_some());
        assert!(sr.waiting_provider.is_none());
    }

    #[test]
    fn step_waiting_is_idempotent() {
        let mut run = fresh_run(&["wait_step"]);
        run.step_waiting("wait_step".into(), "github_app".into())
            .did_execute();
        let outcome = run.step_waiting("wait_step".into(), "github_app".into());
        assert!(matches!(outcome, Idempotent::AlreadyApplied));
    }

    #[test]
    fn step_resumed_is_idempotent() {
        let mut run = fresh_run(&["wait_step"]);
        run.step_waiting("wait_step".into(), "github_app".into())
            .did_execute();
        run.step_resumed("wait_step".into(), json!({}), "provider:github_app".into())
            .did_execute();
        let outcome = run.step_resumed("wait_step".into(), json!({}), "provider:github_app".into());
        assert!(matches!(outcome, Idempotent::AlreadyApplied));
    }

    #[test]
    fn step_waiting_and_resumed_hydrate_from_events() {
        let mut run = fresh_run(&["wait_step"]);
        run.step_waiting("wait_step".into(), "github_app".into())
            .did_execute();
        let output = json!({ "reviewer": "bob" });
        run.step_resumed(
            "wait_step".into(),
            output.clone(),
            "provider:github_app".into(),
        )
        .did_execute();

        let events = run.events;
        let rehydrated = WorkflowRun::try_from_events(events).unwrap();
        assert_eq!(rehydrated.state, WorkflowRunState::Running);
        assert_eq!(rehydrated.step_results.len(), 1);
        let sr = &rehydrated.step_results[0];
        assert_eq!(sr.output.as_ref(), Some(&output));
        assert!(sr.completed_at.is_some());
        assert!(sr.waiting_provider.is_none());
    }

    #[test]
    fn step_waiting_hydrates_as_waiting_for_event() {
        let mut run = fresh_run(&["wait_step"]);
        run.step_waiting("wait_step".into(), "github_app".into())
            .did_execute();

        let events = run.events;
        let rehydrated = WorkflowRun::try_from_events(events).unwrap();
        assert_eq!(rehydrated.state, WorkflowRunState::WaitingForEvent);
        assert_eq!(rehydrated.step_results.len(), 1);
        assert_eq!(
            rehydrated.step_results[0].waiting_provider.as_deref(),
            Some("github_app")
        );
        assert!(rehydrated.step_results[0].output.is_none());
    }

    #[test]
    fn run_completed_is_noop_when_waiting_for_event() {
        let mut run = fresh_run(&["wait_step"]);
        run.step_waiting("wait_step".into(), "github_app".into())
            .did_execute();
        let outcome = run.run_completed();
        assert!(matches!(outcome, Idempotent::AlreadyApplied));
        assert_eq!(run.state, WorkflowRunState::WaitingForEvent);
        assert!(run.completed_at.is_none());
    }

    #[test]
    fn current_wait_step_returns_waiting_step() {
        let mut run = fresh_run(&["a", "wait_step"]);
        start(&mut run, "a");
        complete(&mut run, "a", json!({ "success": true }));
        run.step_waiting("wait_step".into(), "github_app".into())
            .did_execute();

        let ws = run.current_wait_step().expect("should find waiting step");
        assert_eq!(ws.name, "wait_step");
        assert_eq!(ws.waiting_provider.as_deref(), Some("github_app"));
    }

    #[test]
    fn current_wait_step_returns_none_when_not_waiting() {
        let mut run = fresh_run(&["a"]);
        start(&mut run, "a");
        complete(&mut run, "a", json!({ "success": true }));
        assert!(run.current_wait_step().is_none());
    }

    #[test]
    fn full_lifecycle_pending_running_waiting_running_succeeded() {
        let mut run = fresh_run(&["pre", "wait_step", "post"]);

        start(&mut run, "pre");
        complete(&mut run, "pre", json!({ "success": true }));
        assert_eq!(run.state, WorkflowRunState::Running);

        run.step_waiting("wait_step".into(), "github_app".into())
            .did_execute();
        assert_eq!(run.state, WorkflowRunState::WaitingForEvent);

        run.step_resumed(
            "wait_step".into(),
            json!({ "approved": true }),
            "provider:github_app".into(),
        )
        .did_execute();
        assert_eq!(run.state, WorkflowRunState::Running);

        start(&mut run, "post");
        complete(&mut run, "post", json!({ "success": true }));
        assert_eq!(finalize(&mut run), WorkflowRunState::Succeeded);
    }

    #[test]
    fn step_waiting_event_wire_format() {
        let ev = WorkflowRunEvent::StepWaiting {
            step_name: "approval".into(),
            provider: "github_app".into(),
            started_at: Utc::now(),
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v.get("type").and_then(|t| t.as_str()), Some("step_waiting"));
        assert_eq!(
            v.get("provider").and_then(|p| p.as_str()),
            Some("github_app")
        );
    }

    #[test]
    fn step_resumed_event_wire_format() {
        let ev = WorkflowRunEvent::StepResumed {
            step_name: "approval".into(),
            output: json!({ "reviewer": "alice" }),
            source: "provider:github_app".into(),
            resumed_at: Utc::now(),
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v.get("type").and_then(|t| t.as_str()), Some("step_resumed"));
        assert_eq!(
            v.get("source").and_then(|s| s.as_str()),
            Some("provider:github_app")
        );
    }

    #[test]
    fn step_state_waiting_for_event() {
        let sr = StepResult {
            name: "w".into(),
            output: None,
            error: None,
            completed_at: None,
            skipped: None,
            waiting_provider: Some("github_app".into()),
        };
        assert_eq!(sr.step_state(), WorkflowStepState::WaitingForEvent);
    }

    // ── Loop step tests ──

    #[test]
    fn loop_iteration_started_event_wire_format() {
        let ev = WorkflowRunEvent::LoopIterationStarted {
            step_name: "review_cycle".into(),
            iteration: 1,
            started_at: Utc::now(),
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(
            v.get("type").and_then(|t| t.as_str()),
            Some("loop_iteration_started")
        );
        assert_eq!(v.get("iteration").and_then(|i| i.as_u64()), Some(1));
    }

    #[test]
    fn loop_iteration_completed_event_wire_format() {
        let ev = WorkflowRunEvent::LoopIterationCompleted {
            step_name: "review_cycle".into(),
            iteration: 2,
            broke: true,
            completed_at: Utc::now(),
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(
            v.get("type").and_then(|t| t.as_str()),
            Some("loop_iteration_completed")
        );
        assert_eq!(v.get("broke").and_then(|b| b.as_bool()), Some(true));
        assert_eq!(v.get("iteration").and_then(|i| i.as_u64()), Some(2));
    }

    #[test]
    fn loop_iteration_started_is_idempotent_per_iteration() {
        let mut run = fresh_run(&["loop_step"]);
        run.loop_iteration_started("loop_step".into(), 1)
            .did_execute();
        let outcome = run.loop_iteration_started("loop_step".into(), 1);
        assert!(matches!(outcome, Idempotent::AlreadyApplied));
        // Next iteration is not blocked.
        run.loop_iteration_started("loop_step".into(), 2)
            .did_execute();
        assert_eq!(run.loop_current_iteration("loop_step"), 2);
    }

    #[test]
    fn loop_iteration_completed_is_idempotent_per_iteration() {
        let mut run = fresh_run(&["loop_step"]);
        run.loop_iteration_started("loop_step".into(), 1)
            .did_execute();
        run.loop_iteration_completed("loop_step".into(), 1, false)
            .did_execute();
        let outcome = run.loop_iteration_completed("loop_step".into(), 1, false);
        assert!(matches!(outcome, Idempotent::AlreadyApplied));
    }

    #[test]
    fn loop_current_iteration_returns_highest_started() {
        let mut run = fresh_run(&["loop_step"]);
        assert_eq!(run.loop_current_iteration("loop_step"), 0);
        run.loop_iteration_started("loop_step".into(), 1)
            .did_execute();
        assert_eq!(run.loop_current_iteration("loop_step"), 1);
        run.loop_iteration_started("loop_step".into(), 2)
            .did_execute();
        assert_eq!(run.loop_current_iteration("loop_step"), 2);
    }

    #[test]
    fn loop_broke_is_false_until_broke_event_recorded() {
        let mut run = fresh_run(&["loop_step"]);
        assert!(!run.loop_broke("loop_step"));
        run.loop_iteration_started("loop_step".into(), 1)
            .did_execute();
        run.loop_iteration_completed("loop_step".into(), 1, false)
            .did_execute();
        assert!(!run.loop_broke("loop_step"));
        run.loop_iteration_started("loop_step".into(), 2)
            .did_execute();
        run.loop_iteration_completed("loop_step".into(), 2, true)
            .did_execute();
        assert!(run.loop_broke("loop_step"));
    }

    #[test]
    fn loop_events_hydrate_with_running_state() {
        let mut run = fresh_run(&["loop_step"]);
        run.loop_iteration_started("loop_step".into(), 1)
            .did_execute();
        run.loop_iteration_completed("loop_step".into(), 1, true)
            .did_execute();
        let events = run.events;
        let rehydrated = WorkflowRun::try_from_events(events).unwrap();
        assert_eq!(rehydrated.state, WorkflowRunState::Running);
        assert_eq!(rehydrated.loop_current_iteration("loop_step"), 1);
        assert!(rehydrated.loop_broke("loop_step"));
    }

    /// End-to-end: a Loop step over two iterations. Inner step
    /// records use the dotted-name shape produced by the executor
    /// (`{loop}.iter[{N}].{inner}`). After completion,
    /// `loop_iterations_so_far` returns one row per iteration with
    /// each inner step's output keyed by its inner name and the
    /// `broke` flag from the corresponding `LoopIterationCompleted`
    /// event. Mirrors what the executor builds at break_condition
    /// time and what the outer `StepResult.output` would surface.
    #[test]
    fn loop_iterations_so_far_aggregates_two_iterations() {
        let mut run = fresh_run(&["review"]);
        // Outer step starts
        run.step_started("review".into()).did_execute();
        // Iteration 1
        run.loop_iteration_started("review".into(), 1).did_execute();
        run.step_started("review.iter[1].check".into())
            .did_execute();
        run.step_completed("review.iter[1].check".into(), json!({ "merged": false }))
            .did_execute();
        run.loop_iteration_completed("review".into(), 1, false)
            .did_execute();
        // Iteration 2 — break
        run.loop_iteration_started("review".into(), 2).did_execute();
        run.step_started("review.iter[2].check".into())
            .did_execute();
        run.step_completed("review.iter[2].check".into(), json!({ "merged": true }))
            .did_execute();
        run.loop_iteration_completed("review".into(), 2, true)
            .did_execute();

        let iters = run.loop_iterations_so_far("review");
        assert_eq!(iters.len(), 2);
        assert_eq!(iters[0]["iteration"], json!(1));
        assert_eq!(iters[0]["broke"], json!(false));
        assert_eq!(iters[0]["steps"]["check"], json!({ "merged": false }));
        assert_eq!(iters[1]["iteration"], json!(2));
        assert_eq!(iters[1]["broke"], json!(true));
        assert_eq!(iters[1]["steps"]["check"], json!({ "merged": true }));
        assert!(run.loop_broke("review"));
    }

    /// Inner Wait inside Loop parks the run. The waiting step's
    /// name is the dotted form, so a resume that targets it must
    /// resolve back through the Loop's `steps` to find the def
    /// (covered by `lookup_wait_step_def` in mod.rs tests).
    /// Confirms the run state machine here: park flips to
    /// `WaitingForEvent`, resume flips back to `Running`.
    #[test]
    fn loop_inner_wait_parks_and_resumes_via_dotted_name() {
        let mut run = fresh_run(&["review"]);
        run.step_started("review".into()).did_execute();
        run.loop_iteration_started("review".into(), 1).did_execute();
        run.step_waiting("review.iter[1].await_review".into(), "github_app".into())
            .did_execute();
        assert_eq!(run.state, WorkflowRunState::WaitingForEvent);
        let ws = run.current_wait_step().expect("waiting step");
        assert_eq!(ws.name, "review.iter[1].await_review");
        assert_eq!(ws.waiting_provider.as_deref(), Some("github_app"));

        run.step_resumed(
            "review.iter[1].await_review".into(),
            json!({ "action": "approved" }),
            "provider:github_app".into(),
        )
        .did_execute();
        assert_eq!(run.state, WorkflowRunState::Running);
    }
}
