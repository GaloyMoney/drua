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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunState {
    Pending,
    Running,
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
        self.step_results.iter().any(step_reported_agent_failure)
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
            });
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
    /// No-op if the run already reached a terminal state.
    pub fn run_completed(&mut self) -> Idempotent<()> {
        idempotency_guard!(
            self.events.iter_all().rev(),
            already_applied: WorkflowRunEvent::RunCompleted { .. },
        );
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
}

/// True when a cleanly-completed step's structured output carries
/// `success: false` (top-level boolean). Infrastructure-errored and
/// condition-skipped steps return false here — they're classified
/// as `Errored` and Succeeded respectively, never `Failed`.
/// Missing or non-boolean `success` defaults to "no agent-reported
/// failure" (back-compat for non-default-schema steps).
fn step_reported_agent_failure(step: &StepResult) -> bool {
    if step.error.is_some() || step.skipped.is_some() {
        return false;
    }
    let reported_success = step
        .output
        .as_ref()
        .and_then(|v| v.as_object())
        .and_then(|o| o.get("success"))
        .and_then(|s| s.as_bool())
        .unwrap_or(true);
    !reported_success
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
                        });
                    }
                }
                WorkflowRunEvent::StepErrored {
                    step_name,
                    error,
                    completed_at: ts,
                } => {
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
}
