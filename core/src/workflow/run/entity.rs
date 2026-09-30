use chrono::{DateTime, Utc};
use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;

use crate::primitives::*;
use crate::workflow::definition::{SpaceWritesFailure, WorkflowStepDef};

/// Terminal states distinguish how a run ended:
/// - `Succeeded`: every step finished and reported semantic success.
/// - `Failed`: at least one step finished cleanly but the agent
///   self-reported failure via `output.success == false`.
/// - `Errored`: at least one step hit an infrastructure-level error
///   (sandbox not ready, idle timeout, executor / agent error, etc.).
///   Errored takes precedence over Failed.
/// - `Cancelled`: an operator (or a self-cancelling workflow) aborted
///   the run via `Workflows::cancel_run` before it reached one of the
///   above. Distinct so observability + skills can tell a deliberate
///   abort apart from an infrastructure error.
/// - `BudgetExceeded`: the run's `max_cost_usd` was reached (or a
///   bounded attempt's cost couldn't be verified) and the executor
///   stopped admitting further model requests. Distinct from
///   `Errored`/`Cancelled`/a provider 402 mid-`Errored` step — see
///   `handoff-workflow-max-cost-usd-2026-09-30.md` §5. Persisted as
///   `VARCHAR` (no Postgres enum, no CHECK constraint), so this new
///   variant needs no migration.
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
    Cancelled,
    BudgetExceeded,
}

impl WorkflowRunState {
    /// True once the run has reached a state the executor will never
    /// leave. `WaitingForEvent` is deliberately excluded — a parked
    /// run resumes on a matching inbound event.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            WorkflowRunState::Succeeded
                | WorkflowRunState::Failed
                | WorkflowRunState::Errored
                | WorkflowRunState::Cancelled
                | WorkflowRunState::BudgetExceeded
        )
    }
}

/// Why a bounded run's budget stopped dispatch. `LimitReached` is the
/// ordinary case (handoff §3); `CostMeteringUnavailable` is a
/// conservative fail-closed stop — a budget that can't be measured must
/// not silently become unlimited (handoff §4 / P3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetStopReason {
    /// Settled spend reached or exceeded `max_cost_usd`.
    LimitReached,
    /// A step's model dispatch completed (or failed) without the
    /// provider reporting a cost for at least one turn, so this step's
    /// true spend can't be verified.
    CostMeteringUnavailable,
}

/// Total model spend the executor observed for one `execute_step`
/// invocation — the sum of every turn's reported cost across every
/// `stream_agent_response` call it took (initial dispatch, tool-use
/// turns, continuations, forced-output nudges). Enforcement happens per
/// turn inside the agent dispatch loop (via the `remaining` ceiling
/// passed into it); this is the durable, once-per-step settlement the
/// executor folds into `WorkflowRun::spent` when the step returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum StepCost {
    /// Every turn in the step reported a cost (possibly `0` for a
    /// demonstrably unbilled pre-stream failure); this is the sum.
    Known { usd: MicroUsd },
    /// At least one turn completed or failed without a resolvable cost.
    /// Stops a bounded run — see `BudgetStopReason::CostMeteringUnavailable`.
    Unknown,
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
        /// Snapshot of the definition's `max_cost_usd` at trigger time
        /// (handoff §2: "snapshot the limit into the run at
        /// initialization alongside its existing snapshots"). Immutable
        /// for the run's lifetime — editing the definition only affects
        /// future runs. `#[serde(default)]` so pre-existing run events
        /// hydrate with no limit (unbounded), same convention as
        /// `ChangesetOpened.on_failure`.
        #[serde(default)]
        max_cost_usd: Option<f64>,
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
    RunCompleted {
        state: WorkflowRunState,
        completed_at: DateTime<Utc>,
    },
    /// Operator- (or self-) initiated abort. Terminal, and takes
    /// precedence over whatever step state the run was in. `cancelled_by`
    /// is the auth-subject label of the caller; `reason` is an optional
    /// free-text audit note.
    RunCancelled {
        cancelled_by: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        cancelled_at: DateTime<Utc>,
    },
    ChangesetOpened {
        changeset_id: ChangesetId,
        #[serde(default)]
        on_failure: SpaceWritesFailure,
    },
    ChangesetClosed {
        changeset_id: ChangesetId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        outcome: Option<SpaceWritesOutcome>,
    },
    /// Durable settlement of one step's total model spend, folded into
    /// `WorkflowRun::spent` when the executor observes `execute_step`
    /// return. Only emitted for bounded runs (`max_cost_usd.is_some()`)
    /// — an unlimited run never pays this bookkeeping cost. Each event
    /// represents genuinely new spend (turns dispatched during that one
    /// `execute_step` invocation), so replays across job retries of the
    /// same step accumulate correctly rather than needing an
    /// idempotency guard — a step already in a terminal `StepResult`
    /// (`step_already_terminal`) is never re-executed, so this can't
    /// double-count a step that already settled.
    StepModelSpend {
        step_name: String,
        cost: StepCost,
        recorded_at: DateTime<Utc>,
    },
    /// Terminal: the run's budget stopped further model dispatch.
    /// Carries the diagnostic fields handoff §5 asks for. Idempotent —
    /// recorded at most once per run (see
    /// `WorkflowRun::record_budget_stop`).
    BudgetExceeded {
        reason: BudgetStopReason,
        limit: MicroUsd,
        spent: MicroUsd,
        overshoot: MicroUsd,
        step_name: String,
        exceeded_at: DateTime<Utc>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SpaceWritesOutcome {
    Merged { merge_oid: String },
    PrOpened { pr_number: u64, pr_url: String },
    Discarded { reason: String },
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
    #[builder(default)]
    pub changeset: Option<ChangesetId>,
    #[builder(default)]
    pub changeset_on_failure: Option<SpaceWritesFailure>,
    /// The most recently closed draft's id and outcome — unlike
    /// `changeset`/`changeset_on_failure` (cleared on close so the
    /// idempotency check in `changeset_closed` works), this is kept
    /// around for the run's own final output (OQ-12,
    /// handoff-space-changesets-followups-2026-09-28.md).
    #[builder(default)]
    pub last_changeset_id: Option<ChangesetId>,
    #[builder(default)]
    pub last_changeset_outcome: Option<SpaceWritesOutcome>,
    /// Immutable snapshot of the definition's budget at trigger time.
    /// `None` is unlimited.
    #[builder(default)]
    pub max_cost_usd: Option<f64>,
    /// Sum of every step's `StepCost::Known` settlement on this run.
    /// Only meaningful (and only ever nonzero) when `max_cost_usd.is_some()`.
    #[builder(default)]
    pub spent: MicroUsd,
    /// Populated once the run's budget threshold is crossed.
    #[builder(default)]
    pub budget_stop: Option<BudgetStop>,
    events: EntityEvents<WorkflowRunEvent>,
}

/// Diagnostic snapshot of why/where a bounded run stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetStop {
    pub reason: BudgetStopReason,
    pub limit: MicroUsd,
    pub spent: MicroUsd,
    pub overshoot: MicroUsd,
}

impl WorkflowRun {
    /// USD view of [`Self::spent`] for status/API surfaces.
    pub fn spent_usd(&self) -> f64 {
        self.spent.as_dollars()
    }

    /// `max(0, limit - spent)`, or `None` for an unlimited run (handoff
    /// §5: "For unlimited runs remaining budget is null").
    pub fn remaining_cost_usd(&self) -> Option<f64> {
        self.remaining_budget().map(MicroUsd::as_dollars)
    }

    /// Idempotently records why a bounded run's budget stopped (first
    /// caller wins — a later crossing is a no-op) and transitions the
    /// run to `BudgetExceeded`.
    fn record_budget_stop(&mut self, reason: BudgetStopReason, limit: MicroUsd, step_name: String) {
        if self.budget_stop.is_some() {
            return;
        }
        let overshoot = self.spent.saturating_sub(limit);
        let stop = BudgetStop {
            reason,
            limit,
            spent: self.spent,
            overshoot,
        };
        self.budget_stop = Some(stop);
        self.state = WorkflowRunState::BudgetExceeded;
        let now = Utc::now();
        self.completed_at = Some(now);
        self.events.push(WorkflowRunEvent::BudgetExceeded {
            reason,
            limit,
            spent: stop.spent,
            overshoot,
            step_name,
            exceeded_at: now,
        });
    }

    /// Folds one step's total observed model spend into the run's
    /// durable ledger (handoff §3-4). Called by the executor once
    /// `execute_step` returns (success or failure — a step that erred
    /// out after spending money still owes that spend), never from
    /// inside the agent dispatch path itself, so there is exactly one
    /// writer of `WorkflowRun` and no optimistic-concurrency race with
    /// the executor's own step-transition events. A no-op on an
    /// unlimited run (`max_cost_usd.is_none()`) — unbounded runs never
    /// pay this bookkeeping cost. `Known` sums into `spent`, checking
    /// the threshold; `Unknown` stops the run outright — a budget that
    /// can't be measured must not silently become unlimited (handoff §4).
    pub fn record_step_spend(&mut self, step_name: String, cost: StepCost) {
        let Some(limit_usd) = self.max_cost_usd else {
            return;
        };
        let limit = MicroUsd::from_validated_limit(limit_usd);
        self.events.push(WorkflowRunEvent::StepModelSpend {
            step_name: step_name.clone(),
            cost,
            recorded_at: Utc::now(),
        });
        match cost {
            StepCost::Known { usd } => {
                self.spent = self.spent.saturating_add(usd);
                if self.spent >= limit {
                    self.record_budget_stop(BudgetStopReason::LimitReached, limit, step_name);
                }
            }
            StepCost::Unknown => {
                self.record_budget_stop(
                    BudgetStopReason::CostMeteringUnavailable,
                    limit,
                    step_name,
                );
            }
        }
    }

    /// `true` once this run's budget has stopped it — checked by the
    /// executor before dispatching the next step (handoff §3: "no
    /// further admitted requests once the threshold is reached").
    pub fn budget_stopped(&self) -> bool {
        self.budget_stop.is_some()
    }

    /// Remaining room under this run's budget as of right now, or `None`
    /// for an unlimited run. The executor passes this into the agent
    /// dispatch path as the per-turn admission ceiling (handoff §3) —
    /// `Some(MicroUsd::ZERO)` admits no further model requests.
    pub fn remaining_budget(&self) -> Option<MicroUsd> {
        let limit = MicroUsd::from_validated_limit(self.max_cost_usd?);
        Some(limit.saturating_sub(self.spent))
    }

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

    /// `false` once a bounded run's budget has stopped it, even if every
    /// step so far reported clean success — a budget stop must route to
    /// `finish_space_writes`'s abandon path (keep/discard), never to the
    /// success/merge path (handoff §5).
    pub fn would_succeed(&self) -> bool {
        self.budget_stop.is_none() && !self.any_step_errored() && !self.any_step_reported_failure()
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
            already_applied: WorkflowRunEvent::RunCancelled { .. },
            already_applied: WorkflowRunEvent::BudgetExceeded { .. },
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

    /// Aborts the run: records a terminal `Cancelled` state regardless
    /// of which step was in flight. No-op if the run already reached any
    /// terminal state (idempotent against retries and races with
    /// `run_completed` — whichever terminal event lands first wins).
    ///
    /// The executor observes the persisted terminal state on its next
    /// job attempt and exits without further work; the queue slot frees
    /// once that attempt completes.
    pub fn cancel(&mut self, cancelled_by: String, reason: Option<String>) -> Idempotent<()> {
        idempotency_guard!(
            self.events.iter_all().rev(),
            already_applied: WorkflowRunEvent::RunCompleted { .. },
            already_applied: WorkflowRunEvent::RunCancelled { .. },
            already_applied: WorkflowRunEvent::BudgetExceeded { .. },
        );
        let now = Utc::now();
        self.state = WorkflowRunState::Cancelled;
        self.completed_at = Some(now);
        self.events.push(WorkflowRunEvent::RunCancelled {
            cancelled_by,
            reason,
            cancelled_at: now,
        });
        Idempotent::Executed(())
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

    pub fn changeset_opened(
        &mut self,
        changeset_id: ChangesetId,
        on_failure: SpaceWritesFailure,
    ) -> Idempotent<()> {
        idempotency_guard!(
            self.events.iter_all().rev(),
            already_applied: WorkflowRunEvent::ChangesetOpened { .. },
        );
        self.changeset = Some(changeset_id);
        self.changeset_on_failure = Some(on_failure);
        self.events.push(WorkflowRunEvent::ChangesetOpened {
            changeset_id,
            on_failure,
        });
        Idempotent::Executed(())
    }

    pub fn changeset_closed(
        &mut self,
        changeset_id: ChangesetId,
        outcome: Option<SpaceWritesOutcome>,
    ) -> Idempotent<()> {
        if self.changeset != Some(changeset_id) {
            return Idempotent::AlreadyApplied;
        }
        self.changeset = None;
        self.changeset_on_failure = None;
        self.last_changeset_id = Some(changeset_id);
        self.last_changeset_outcome = outcome.clone();
        self.events.push(WorkflowRunEvent::ChangesetClosed {
            changeset_id,
            outcome,
        });
        Idempotent::Executed(())
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
        let mut changeset: Option<ChangesetId> = None;
        let mut changeset_on_failure: Option<SpaceWritesFailure> = None;
        let mut last_changeset_id: Option<ChangesetId> = None;
        let mut last_changeset_outcome: Option<SpaceWritesOutcome> = None;
        let mut max_cost_usd: Option<f64> = None;
        let mut spent = MicroUsd::ZERO;
        let mut budget_stop: Option<BudgetStop> = None;

        for event in events.iter_all() {
            match event {
                WorkflowRunEvent::Initialized {
                    id,
                    definition_id,
                    project_id,
                    trigger_context,
                    steps_snapshot,
                    max_cost_usd: limit,
                } => {
                    max_cost_usd = *limit;
                    builder = builder
                        .id(*id)
                        .definition_id(*definition_id)
                        .project_id(*project_id)
                        .trigger_context(trigger_context.clone())
                        .steps_snapshot(steps_snapshot.clone());
                }
                WorkflowRunEvent::StepStarted { step_name, .. } => {
                    if state != WorkflowRunState::BudgetExceeded {
                        state = WorkflowRunState::Running;
                    }
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
                    // Doesn't downgrade an already-recorded `BudgetExceeded`
                    // (or any other terminal state a future event type might
                    // add) — this event's own `state` field can land AFTER
                    // the run's terminal transition in the persisted stream
                    // (the executor folds a step's spend, which may cross
                    // the budget, before or after the step's own outcome —
                    // see `Executor::run`), so hydration must not assume a
                    // step event is always the most authoritative signal.
                    if state != WorkflowRunState::BudgetExceeded {
                        state = WorkflowRunState::Running;
                    }
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
                    if state != WorkflowRunState::BudgetExceeded {
                        state = WorkflowRunState::Running;
                    }
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
                WorkflowRunEvent::StepWaiting {
                    step_name,
                    provider,
                    ..
                } => {
                    if state != WorkflowRunState::BudgetExceeded {
                        state = WorkflowRunState::WaitingForEvent;
                    }
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
                    if state != WorkflowRunState::BudgetExceeded {
                        state = WorkflowRunState::Running;
                    }
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
                WorkflowRunEvent::RunCancelled { cancelled_at, .. } => {
                    state = WorkflowRunState::Cancelled;
                    completed_at = Some(*cancelled_at);
                }
                WorkflowRunEvent::ChangesetOpened {
                    changeset_id,
                    on_failure,
                } => {
                    changeset = Some(*changeset_id);
                    changeset_on_failure = Some(*on_failure);
                }
                WorkflowRunEvent::ChangesetClosed {
                    changeset_id,
                    outcome,
                } => {
                    if changeset == Some(*changeset_id) {
                        changeset = None;
                        changeset_on_failure = None;
                    }
                    last_changeset_id = Some(*changeset_id);
                    last_changeset_outcome = outcome.clone();
                }
                WorkflowRunEvent::StepModelSpend { cost, .. } => {
                    if let StepCost::Known { usd } = cost {
                        spent = spent.saturating_add(*usd);
                    }
                }
                WorkflowRunEvent::BudgetExceeded {
                    reason,
                    limit,
                    spent: settled_spent,
                    overshoot,
                    exceeded_at,
                    ..
                } => {
                    state = WorkflowRunState::BudgetExceeded;
                    completed_at = Some(*exceeded_at);
                    budget_stop = Some(BudgetStop {
                        reason: *reason,
                        limit: *limit,
                        spent: *settled_spent,
                        overshoot: *overshoot,
                    });
                }
            }
        }

        builder = builder.state(state).step_results(results);
        builder = builder.completed_at(completed_at);
        builder = builder.changeset(changeset);
        builder = builder.changeset_on_failure(changeset_on_failure);
        builder = builder.last_changeset_id(last_changeset_id);
        builder = builder.last_changeset_outcome(last_changeset_outcome);
        builder = builder.max_cost_usd(max_cost_usd);
        builder = builder.spent(spent);
        builder = builder.budget_stop(budget_stop);

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
    #[builder(default)]
    pub(crate) max_cost_usd: Option<f64>,
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
                max_cost_usd: self.max_cost_usd,
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

    // ── Cancel tests ──

    #[test]
    fn cancel_transitions_running_to_cancelled() {
        let mut run = fresh_run(&["a"]);
        start(&mut run, "a");
        assert_eq!(run.state, WorkflowRunState::Running);

        assert!(run
            .cancel("user:abc".into(), Some("stuck".into()))
            .did_execute());
        assert_eq!(run.state, WorkflowRunState::Cancelled);
        assert!(run.completed_at.is_some());
        assert!(run.state.is_terminal());
    }

    #[test]
    fn cancel_from_waiting_for_event() {
        let mut run = fresh_run(&["wait_step"]);
        run.step_waiting("wait_step".into(), "github_app".into())
            .did_execute();
        assert_eq!(run.state, WorkflowRunState::WaitingForEvent);

        assert!(run.cancel("user:abc".into(), None).did_execute());
        assert_eq!(run.state, WorkflowRunState::Cancelled);
    }

    #[test]
    fn cancel_is_idempotent_against_retry() {
        let mut run = fresh_run(&["a"]);
        run.cancel("user:abc".into(), None).did_execute();
        let outcome = run.cancel("user:xyz".into(), Some("again".into()));
        assert!(matches!(outcome, Idempotent::AlreadyApplied));
        assert_eq!(run.state, WorkflowRunState::Cancelled);
    }

    #[test]
    fn cancel_is_noop_on_already_completed_run() {
        let mut run = fresh_run(&["a"]);
        start(&mut run, "a");
        complete(&mut run, "a", json!({ "success": true }));
        finalize(&mut run);
        assert_eq!(run.state, WorkflowRunState::Succeeded);

        let outcome = run.cancel("user:abc".into(), None);
        assert!(matches!(outcome, Idempotent::AlreadyApplied));
        // The earlier terminal state wins — cancel does not overwrite it.
        assert_eq!(run.state, WorkflowRunState::Succeeded);
    }

    #[test]
    fn run_completed_is_noop_after_cancel() {
        let mut run = fresh_run(&["a"]);
        run.cancel("user:abc".into(), None).did_execute();
        let outcome = run.run_completed();
        assert!(matches!(outcome, Idempotent::AlreadyApplied));
        assert_eq!(run.state, WorkflowRunState::Cancelled);
    }

    #[test]
    fn cancel_hydrates_from_events() {
        let mut run = fresh_run(&["a"]);
        start(&mut run, "a");
        run.cancel("user:abc".into(), Some("git-proxy down".into()))
            .did_execute();
        let events = run.events;
        let rehydrated = WorkflowRun::try_from_events(events).unwrap();
        assert_eq!(rehydrated.state, WorkflowRunState::Cancelled);
        assert!(rehydrated.completed_at.is_some());
    }

    #[test]
    fn cancel_event_wire_format() {
        let ev = WorkflowRunEvent::RunCancelled {
            cancelled_by: "user:abc".into(),
            reason: Some("stuck".into()),
            cancelled_at: Utc::now(),
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(
            v.get("type").and_then(|t| t.as_str()),
            Some("run_cancelled")
        );
        assert_eq!(
            v.get("cancelled_by").and_then(|c| c.as_str()),
            Some("user:abc")
        );
    }

    #[test]
    fn changeset_opened_and_closed_round_trip() {
        let mut run = fresh_run(&["a"]);
        assert!(run.changeset.is_none());

        let id = ChangesetId::new();
        assert!(run
            .changeset_opened(id, SpaceWritesFailure::Keep)
            .did_execute());
        assert_eq!(run.changeset, Some(id));
        assert_eq!(run.changeset_on_failure, Some(SpaceWritesFailure::Keep));

        assert!(run
            .changeset_closed(
                id,
                Some(SpaceWritesOutcome::Merged {
                    merge_oid: "abc123".into()
                })
            )
            .did_execute());
        assert!(run.changeset.is_none());
        assert!(run.changeset_on_failure.is_none());
        // Unlike `changeset`/`changeset_on_failure`, these survive the
        // close so the run's final output can report the outcome
        // (OQ-12).
        assert_eq!(run.last_changeset_id, Some(id));
        assert_eq!(
            run.last_changeset_outcome,
            Some(SpaceWritesOutcome::Merged {
                merge_oid: "abc123".into()
            })
        );
    }

    #[test]
    fn last_changeset_outcome_hydrates_from_events() {
        let mut run = fresh_run(&["a"]);
        let id = ChangesetId::new();
        run.changeset_opened(id, SpaceWritesFailure::Discard)
            .did_execute();
        run.changeset_closed(
            id,
            Some(SpaceWritesOutcome::PrOpened {
                pr_number: 42,
                pr_url: "https://github.com/x/y/pull/42".into(),
            }),
        )
        .did_execute();

        let events = run.events;
        let rehydrated = WorkflowRun::try_from_events(events).unwrap();
        assert!(rehydrated.changeset.is_none());
        assert_eq!(rehydrated.last_changeset_id, Some(id));
        assert_eq!(
            rehydrated.last_changeset_outcome,
            Some(SpaceWritesOutcome::PrOpened {
                pr_number: 42,
                pr_url: "https://github.com/x/y/pull/42".into(),
            })
        );
    }

    #[test]
    fn changeset_closed_retry_after_close_is_idempotent_and_keeps_last_outcome() {
        // `changeset_closed` called twice with the same id (e.g. a
        // retried `finish_space_writes`) must not push a second event
        // — the `self.changeset != Some(id)` guard already covered
        // this; confirm `last_changeset_*` doesn't get clobbered by a
        // no-op retry either.
        let mut run = fresh_run(&["a"]);
        let id = ChangesetId::new();
        run.changeset_opened(id, SpaceWritesFailure::Discard)
            .did_execute();
        run.changeset_closed(
            id,
            Some(SpaceWritesOutcome::Discarded {
                reason: "empty".into(),
            }),
        )
        .did_execute();

        let retry = run.changeset_closed(
            id,
            Some(SpaceWritesOutcome::Merged {
                merge_oid: "should-not-apply".into(),
            }),
        );
        assert!(matches!(retry, Idempotent::AlreadyApplied));
        assert_eq!(
            run.last_changeset_outcome,
            Some(SpaceWritesOutcome::Discarded {
                reason: "empty".into(),
            })
        );
    }

    #[test]
    fn changeset_opened_is_idempotent_against_retry() {
        let mut run = fresh_run(&["a"]);
        let id = ChangesetId::new();
        run.changeset_opened(id, SpaceWritesFailure::Discard)
            .did_execute();
        let outcome = run.changeset_opened(id, SpaceWritesFailure::Discard);
        assert!(matches!(outcome, Idempotent::AlreadyApplied));
        assert_eq!(run.changeset, Some(id));
    }

    #[test]
    fn changeset_closed_wrong_id_is_noop() {
        let mut run = fresh_run(&["a"]);
        let opened = ChangesetId::new();
        let other = ChangesetId::new();
        run.changeset_opened(opened, SpaceWritesFailure::Keep)
            .did_execute();

        let outcome = run.changeset_closed(
            other,
            Some(SpaceWritesOutcome::Discarded {
                reason: "no space writes".into(),
            }),
        );
        assert!(matches!(outcome, Idempotent::AlreadyApplied));
        assert_eq!(run.changeset, Some(opened));
        assert_eq!(run.changeset_on_failure, Some(SpaceWritesFailure::Keep));
    }

    #[test]
    fn changeset_binding_hydrates_from_events() {
        let mut run = fresh_run(&["a"]);
        let id = ChangesetId::new();
        run.changeset_opened(id, SpaceWritesFailure::Keep)
            .did_execute();

        let events = run.events;
        let rehydrated = WorkflowRun::try_from_events(events).unwrap();
        assert_eq!(rehydrated.changeset, Some(id));
        assert_eq!(
            rehydrated.changeset_on_failure,
            Some(SpaceWritesFailure::Keep)
        );
    }

    #[test]
    fn changeset_closed_hydrates_as_cleared() {
        let mut run = fresh_run(&["a"]);
        let id = ChangesetId::new();
        run.changeset_opened(id, SpaceWritesFailure::Keep)
            .did_execute();
        run.changeset_closed(
            id,
            Some(SpaceWritesOutcome::PrOpened {
                pr_number: 7,
                pr_url: "https://example.com/pr/7".into(),
            }),
        )
        .did_execute();

        let events = run.events;
        let rehydrated = WorkflowRun::try_from_events(events).unwrap();
        assert!(rehydrated.changeset.is_none());
        assert!(rehydrated.changeset_on_failure.is_none());
    }

    #[test]
    fn pre_rev5_changeset_events_hydrate_with_defaults() {
        let id = ChangesetId::new();
        let opened = serde_json::json!({
            "type": "changeset_opened",
            "changeset_id": id,
        });
        let closed = serde_json::json!({
            "type": "changeset_closed",
            "changeset_id": id,
        });
        let opened: WorkflowRunEvent = serde_json::from_value(opened).unwrap();
        let closed: WorkflowRunEvent = serde_json::from_value(closed).unwrap();
        assert!(matches!(
            opened,
            WorkflowRunEvent::ChangesetOpened {
                on_failure: SpaceWritesFailure::Discard,
                ..
            }
        ));
        assert!(matches!(
            closed,
            WorkflowRunEvent::ChangesetClosed { outcome: None, .. }
        ));
    }

    // -- max_cost_usd budget ledger (handoff-workflow-max-cost-usd-2026-09-30.md) --

    fn fresh_bounded_run(step_names: &[&str], max_cost_usd: f64) -> WorkflowRun {
        let new = NewWorkflowRun::builder()
            .definition_id(WorkflowDefinitionId::new())
            .project_id(ProjectId::new())
            .trigger_context(json!({}))
            .steps_snapshot(step_names.iter().map(|n| sample_step(n)).collect())
            .max_cost_usd(Some(max_cost_usd))
            .build()
            .unwrap();
        WorkflowRun::try_from_events(new.into_events()).unwrap()
    }

    #[test]
    fn unbounded_run_has_no_remaining_budget_and_ignores_spend() {
        let mut run = fresh_run(&["step"]);
        assert_eq!(run.max_cost_usd, None);
        assert_eq!(run.remaining_budget(), None);
        assert_eq!(run.remaining_cost_usd(), None);

        run.record_step_spend(
            "step".into(),
            StepCost::Known {
                usd: MicroUsd::from_settled_cost(1_000.0),
            },
        );
        assert_eq!(run.spent, MicroUsd::ZERO);
        assert!(!run.budget_stopped());
        assert_eq!(run.state, WorkflowRunState::Pending);
    }

    #[test]
    fn zero_limit_run_has_zero_remaining_budget_before_any_spend() {
        // handoff §2: "0 admits no model requests" — the executor reads
        // `remaining_budget()` before ever starting an agent step's
        // dispatch loop, so this must already be `ZERO`, not merely
        // becoming zero after a first (rejected) charge.
        let run = fresh_bounded_run(&["step"], 0.0);
        assert_eq!(run.remaining_budget(), Some(MicroUsd::ZERO));
    }

    #[test]
    fn zero_limit_stops_on_first_known_zero_spend() {
        // Mirrors the ceiling short-circuit in `drive_session_loop`: a
        // `0` ceiling settles as `Known(0)`, not `Unknown` — genuinely no
        // money was at risk, so the stop reason is `LimitReached`, not a
        // metering failure.
        let mut run = fresh_bounded_run(&["step"], 0.0);
        run.record_step_spend(
            "step".into(),
            StepCost::Known {
                usd: MicroUsd::ZERO,
            },
        );
        assert_eq!(run.state, WorkflowRunState::BudgetExceeded);
        let stop = run.budget_stop.expect("budget_stop recorded");
        assert_eq!(stop.reason, BudgetStopReason::LimitReached);
        assert_eq!(stop.overshoot, MicroUsd::ZERO);
        assert!(run.state.is_terminal());
    }

    #[test]
    fn exact_boundary_stops_with_zero_overshoot() {
        let mut run = fresh_bounded_run(&["step"], 5.00);
        run.record_step_spend(
            "step".into(),
            StepCost::Known {
                usd: MicroUsd::from_settled_cost(5.00),
            },
        );
        assert_eq!(run.state, WorkflowRunState::BudgetExceeded);
        assert_eq!(run.spent_usd(), 5.00);
        let stop = run.budget_stop.expect("budget_stop recorded");
        assert_eq!(stop.reason, BudgetStopReason::LimitReached);
        assert_eq!(stop.overshoot, MicroUsd::ZERO);
        assert_eq!(run.remaining_cost_usd(), Some(0.0));
    }

    #[test]
    fn overshoot_example_from_handoff() {
        // $4.80 admitted, next turn settles at $0.35 -> $5.15 spent,
        // $0.15 overshoot (handoff §3's worked example).
        let mut run = fresh_bounded_run(&["step"], 5.00);
        run.record_step_spend(
            "step".into(),
            StepCost::Known {
                usd: MicroUsd::from_settled_cost(4.80),
            },
        );
        assert!(!run.budget_stopped());
        run.record_step_spend(
            "step".into(),
            StepCost::Known {
                usd: MicroUsd::from_settled_cost(0.35),
            },
        );
        assert!(run.budget_stopped());
        assert!((run.spent_usd() - 5.15).abs() < 1e-9);
        let stop = run.budget_stop.expect("budget_stop recorded");
        assert_eq!(stop.reason, BudgetStopReason::LimitReached);
        assert!((stop.overshoot.as_dollars() - 0.15).abs() < 1e-9);
    }

    #[test]
    fn unknown_cost_stops_with_metering_reason_not_limit_reached() {
        // handoff §4/P3: an unpriced turn (any direct-Anthropic/OpenAI
        // response, or a healthy router fallback landing on one) must
        // block further dispatch even though it may be nowhere near the
        // dollar limit — distinct from an ordinary threshold crossing.
        let mut run = fresh_bounded_run(&["step"], 5.00);
        run.record_step_spend("step".into(), StepCost::Unknown);
        assert_eq!(run.state, WorkflowRunState::BudgetExceeded);
        let stop = run.budget_stop.expect("budget_stop recorded");
        assert_eq!(stop.reason, BudgetStopReason::CostMeteringUnavailable);
        // No known charge — spend stays at whatever it was (zero here).
        assert_eq!(run.spent, MicroUsd::ZERO);
    }

    #[test]
    fn spend_accumulates_across_separate_steps() {
        // The run-level total is what's enforced, not a per-step or
        // per-thread figure — mirrors the curate-live evidence
        // (spend split across an initial thread and a post-refresh one,
        // both attributable to the same run).
        let mut run = fresh_bounded_run(&["a", "b", "c"], 10.00);
        run.record_step_spend(
            "a".into(),
            StepCost::Known {
                usd: MicroUsd::from_settled_cost(0.0033),
            },
        );
        run.record_step_spend(
            "b".into(),
            StepCost::Known {
                usd: MicroUsd::from_settled_cost(6.2648),
            },
        );
        assert!(!run.budget_stopped(), "well under the $10 limit so far");
        assert!((run.spent_usd() - 6.2681).abs() < 1e-6);

        run.record_step_spend(
            "c".into(),
            StepCost::Known {
                usd: MicroUsd::from_settled_cost(4.0),
            },
        );
        assert!(run.budget_stopped());
    }

    #[test]
    fn budget_stop_is_recorded_at_most_once() {
        let mut run = fresh_bounded_run(&["a", "b"], 1.00);
        run.record_step_spend(
            "a".into(),
            StepCost::Known {
                usd: MicroUsd::from_settled_cost(2.0),
            },
        );
        let first_stop = run.budget_stop.expect("first stop recorded");
        // A second step somehow still dispatching (shouldn't happen once
        // the executor observes `budget_stopped()`, but the ledger must
        // stay safe if it does) must not overwrite the original stop or
        // keep accumulating `spent` past it in a way that changes the
        // recorded overshoot.
        run.record_step_spend(
            "b".into(),
            StepCost::Known {
                usd: MicroUsd::from_settled_cost(3.0),
            },
        );
        assert_eq!(run.budget_stop, Some(first_stop));
    }

    #[test]
    fn would_succeed_is_false_once_budget_stopped_even_with_clean_steps() {
        let mut run = fresh_bounded_run(&["step"], 1.00);
        complete(&mut run, "step", json!({"success": true}));
        assert!(run.would_succeed());
        run.record_step_spend(
            "step".into(),
            StepCost::Known {
                usd: MicroUsd::from_settled_cost(2.0),
            },
        );
        assert!(
            !run.would_succeed(),
            "a budget stop must never let a clean step read as success"
        );
    }

    #[test]
    fn run_completed_is_a_noop_after_budget_exceeded() {
        let mut run = fresh_bounded_run(&["step"], 1.00);
        complete(&mut run, "step", json!({"success": true}));
        run.record_step_spend(
            "step".into(),
            StepCost::Known {
                usd: MicroUsd::from_settled_cost(2.0),
            },
        );
        assert_eq!(run.state, WorkflowRunState::BudgetExceeded);
        assert!(!run.run_completed().did_execute());
        assert_eq!(
            run.state,
            WorkflowRunState::BudgetExceeded,
            "run_completed must not reclassify a budget-stopped run as Succeeded"
        );
    }

    #[test]
    fn cancel_is_a_noop_after_budget_exceeded() {
        let mut run = fresh_bounded_run(&["step"], 1.00);
        run.record_step_spend("step".into(), StepCost::Unknown);
        assert!(!run.cancel("operator".to_string(), None).did_execute());
        assert_eq!(run.state, WorkflowRunState::BudgetExceeded);
    }

    #[test]
    fn budget_exceeded_state_is_terminal() {
        assert!(WorkflowRunState::BudgetExceeded.is_terminal());
    }

    #[test]
    fn max_cost_usd_and_ledger_hydrate_through_json_round_trip() {
        let mut run = fresh_bounded_run(&["step"], 5.00);
        run.record_step_spend(
            "step".into(),
            StepCost::Known {
                usd: MicroUsd::from_settled_cost(5.50),
            },
        );
        let raw: Vec<serde_json::Value> = run
            .events
            .iter_all()
            .map(|e| serde_json::to_value(e).unwrap())
            .collect();
        let deserialized: Vec<WorkflowRunEvent> = raw
            .into_iter()
            .map(|v| serde_json::from_value(v).unwrap())
            .collect();
        let events = EntityEvents::init(run.id, deserialized);
        let rehydrated = WorkflowRun::try_from_events(events).unwrap();

        assert_eq!(rehydrated.max_cost_usd, Some(5.00));
        assert_eq!(rehydrated.spent, run.spent);
        assert_eq!(rehydrated.state, WorkflowRunState::BudgetExceeded);
        assert_eq!(rehydrated.budget_stop, run.budget_stop);
    }

    #[test]
    fn pre_budget_initialized_event_hydrates_as_unlimited() {
        // Old run events predate `max_cost_usd` entirely — the field
        // must be absent from the wire payload (not merely `null`) and
        // still hydrate cleanly as unlimited (handoff §2).
        let id = WorkflowRunId::new();
        let initialized = serde_json::json!({
            "type": "initialized",
            "id": id,
            "definition_id": WorkflowDefinitionId::new(),
            "project_id": ProjectId::new(),
            "trigger_context": {},
            "steps_snapshot": [],
        });
        let events = EntityEvents::init(
            id,
            [serde_json::from_value::<WorkflowRunEvent>(initialized).unwrap()],
        );
        let run = WorkflowRun::try_from_events(events).unwrap();
        assert_eq!(run.max_cost_usd, None);
        assert_eq!(run.remaining_budget(), None);
    }

    /// Regression: a `StepErrored` (or `StepCompleted`/`StepSkipped`/
    /// `StepWaiting`/`StepResumed`) event persisted AFTER `BudgetExceeded`
    /// in the stream — which happens whenever the executor folds a
    /// step's spend before/after its own outcome — must not hydrate
    /// back to `Running`. Caught by `Executor::run`'s own integration
    /// test the hard way: the in-memory entity transitioned correctly
    /// and persisted `state = 'budget_exceeded'` to the index column,
    /// but reloading via `try_from_events` silently regressed to
    /// `Running` because these fold arms set `state` unconditionally.
    #[test]
    fn step_errored_after_budget_exceeded_does_not_downgrade_state_on_hydration() {
        let mut run = fresh_bounded_run(&["step"], 0.0);
        run.record_step_spend(
            "step".into(),
            StepCost::Known {
                usd: MicroUsd::ZERO,
            },
        );
        assert_eq!(run.state, WorkflowRunState::BudgetExceeded);
        // Mirrors `Executor::run`'s actual event order for a step whose
        // dispatch never got as far as `submit_output`.
        run.step_errored(
            "step".into(),
            "model budget stopped this step before submit_output".into(),
        )
        .did_execute();
        assert_eq!(
            run.state,
            WorkflowRunState::BudgetExceeded,
            "the live command method must not downgrade an already-terminal run either"
        );

        let events = run.events;
        let rehydrated = WorkflowRun::try_from_events(events).unwrap();
        assert_eq!(
            rehydrated.state,
            WorkflowRunState::BudgetExceeded,
            "hydration must not re-derive Running from the StepErrored event"
        );
    }
}
