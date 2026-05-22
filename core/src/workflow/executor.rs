use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::agent::session::message::SUBMIT_OUTPUT_TOOL_NAME;
use crate::agent::{Agent, Agents};
use crate::auth::AuthSubject;
use crate::primitives::{
    AgentId, ChatOutputEvent, ProjectId, SandboxId, WorkflowDefinitionId, WorkflowRunId,
};
use crate::sandbox::{Sandbox, SandboxAgentMode, SandboxSpecs, SandboxState, Sandboxes};
use crate::skill::Skills;
use crate::toolset::ToolSets;

use super::definition::{OutputSchema, WorkflowSandboxDecl, WorkflowStepDef};
use super::error::WorkflowError;
use super::repo::WorkflowDefinitionRepo;
use super::run::{StepResult, WorkflowRun, WorkflowRunRepo, WorkflowRunState};
use super::template::{
    format_template_diagnostics, template_diagnostics, ConditionOutcome, IterationContext,
    TemplateContext,
};

/// Hard cap on the pre-flight per-sandbox readiness wait. Protects the
/// queue from a stuck infrastructure step.
const SANDBOX_READY_TIMEOUT: Duration = Duration::from_secs(180);

/// Result of [`Executor::run_loop_step`]. Three terminal shapes:
///
///   - `Completed` — break_condition fired (`success: true`) or
///     max_iterations exhausted (`success: false`). Caller emits the
///     outer `step_completed` event with the carried output.
///   - `Parked` — an inner Wait step has parked the run. Caller
///     stops processing further outer steps and returns from `run`.
///   - `Cancelled` — cooperative cancellation observed between
///     inner steps; the caller propagates `WorkflowError::Cancelled`.
enum LoopOutcome {
    Completed(serde_json::Value),
    Parked,
    Cancelled,
}

/// Default per-`ToolStep` dispatch timeout when the step doesn't
/// specify one. Same shape as the agent step's idle timeout default.
const DEFAULT_TOOL_STEP_TIMEOUT_SECS: u64 = 300;

/// Build the `steps` slice of the template context from the run's
/// recorded step results. Completed steps surface their structured
/// `output`; skipped / errored / pending steps surface as
/// `Value::Null`. Including the latter keeps CEL from raising
/// `NoSuchKey` when a downstream `${{ steps.<skipped>.outputs.X }}`
/// or `condition: steps.<skipped>.outputs.X` refers to a step that
/// didn't produce one — the access becomes `null.outputs.X` which
/// CEL coerces to `false` per the same quirk documented at
/// `template::TemplateContext::build_cel_context`. Net effect: the
/// "downstream null-coalesce" contract documented in memo
/// `019e06a1` falls out for free.
fn collect_step_outputs(results: &[StepResult]) -> HashMap<String, serde_json::Value> {
    let mut out = HashMap::with_capacity(results.len());
    for r in results {
        match &r.output {
            Some(value) => {
                out.insert(r.name.clone(), value.clone());
            }
            None => {
                out.insert(r.name.clone(), serde_json::Value::Null);
            }
        }
    }
    out
}

fn first_text_content(result: &rmcp::model::CallToolResult) -> Option<String> {
    result.content.iter().find_map(|c| match &c.raw {
        rmcp::model::RawContent::Text(t) => Some(t.text.clone()),
        _ => None,
    })
}

/// Build the agent prompt from a pre-templated skill body and the
/// run's trigger context. The trigger payload is appended as opaque
/// text (or interpolated via `$ARGUMENTS` if the skill opts in) so
/// templates smuggled through user-controlled fields can't reach
/// prior step outputs. Caller is responsible for the substitution
/// pass on the skill body — both `TemplateContext` and
/// `IterationContext` are valid sources.
fn build_agent_prompt(templated_body: String, trigger_context: &serde_json::Value) -> String {
    let pretty = serde_json::to_string_pretty(trigger_context)
        .unwrap_or_else(|_| trigger_context.to_string());
    if templated_body.contains("$ARGUMENTS") {
        crate::skill::SkillBody::new(templated_body).interpolate(Some(&pretty))
    } else {
        format!("{templated_body}\n\nTRIGGER_CONTEXT:\n```json\n{pretty}\n```")
    }
}

/// Per-iteration outputs scoped to a single Loop iteration. Walks
/// the run's `step_results` for entries whose names start with the
/// `{loop_name}.iter[{iteration}].` prefix and returns a map keyed
/// by the inner step name (with the dotted prefix stripped). Used
/// to seed [`crate::workflow::template::IterationContext::iter`]
/// for inner-step condition gates and break_condition evaluation.
fn collect_inner_iter_outputs(
    results: &[StepResult],
    loop_name: &str,
    iteration: u32,
) -> HashMap<String, serde_json::Value> {
    let prefix = format!("{loop_name}.iter[{iteration}].");
    let mut out = HashMap::new();
    for r in results {
        if let Some(stripped) = r.name.strip_prefix(&prefix) {
            let val = r.output.clone().unwrap_or(serde_json::Value::Null);
            out.insert(stripped.to_string(), val);
        }
    }
    out
}

fn type_label(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

pub struct Executor {
    runs: WorkflowRunRepo,
    definitions: WorkflowDefinitionRepo,
    agents: Arc<Agents>,
    skills: Arc<Skills>,
    sandboxes: Arc<Sandboxes>,
    /// Used by `ToolStep` dispatch; held here rather than passed per
    /// call so the agent-step path is unchanged.
    toolsets: Arc<ToolSets>,
}

impl Executor {
    pub fn new(
        runs: WorkflowRunRepo,
        definitions: WorkflowDefinitionRepo,
        agents: Arc<Agents>,
        skills: Arc<Skills>,
        sandboxes: Arc<Sandboxes>,
        toolsets: Arc<ToolSets>,
    ) -> Self {
        Self {
            runs,
            definitions,
            agents,
            skills,
            sandboxes,
            toolsets,
        }
    }

    /// Resumable: idempotent mutations + skipping already-terminal steps
    /// make this safe to invoke repeatedly after a crash.
    /// `Ok(())` whenever a terminal state was recorded; `Err(_)` only on
    /// load/persist failure or cooperative cancellation
    /// ([`WorkflowError::Cancelled`]).
    ///
    /// `cancel` is polled between steps; when set, the run aborts cleanly
    /// without recording `step_errored` so the next attempt resumes the
    /// in-progress step. Callers that don't need cancellation pass a
    /// fresh `Arc<AtomicBool>` initialized to `false`.
    #[tracing::instrument(name = "core.workflow.execute_run", skip_all, fields(run_id = %run_id))]
    pub async fn run(
        &self,
        run_id: WorkflowRunId,
        cancel: Arc<AtomicBool>,
    ) -> Result<(), WorkflowError> {
        let mut run = self.runs.find_by_id(run_id).await?;

        if matches!(
            run.state,
            WorkflowRunState::Succeeded | WorkflowRunState::Failed | WorkflowRunState::Errored
        ) {
            return Ok(());
        }

        let project_id = run.project_id;
        let workflow_id = run.definition_id;
        let trigger_context = run.trigger_context.clone();
        let steps = run.steps_snapshot.clone();

        // Stamp every audit row recorded during this run so they can be
        // queried by `resource_ids->>'workflow_run_id'`.
        crate::audit::Audit::record_project_id(project_id);
        crate::audit::Audit::record_workflow_id(workflow_id);
        crate::audit::Audit::record_workflow_run_id(run_id);

        // Re-read the latest declarations so a recently-edited workflow
        // sees its updated sandbox list on the next trigger.
        let definition = self.definitions.find_by_id(workflow_id).await?;
        let sandbox_decls = definition.sandboxes.clone();

        // Pre-flight: bring all declared sandboxes to Ready. If this
        // fails the run terminates as Failed without running any step.
        // The synthetic `<pre-flight>` step keeps the diagnostic visible
        // in `runs` listings.
        let (sandbox_ids, preexisting_ids) = match self
            .ensure_sandboxes_ready(project_id, workflow_id, &sandbox_decls)
            .await
        {
            Ok(pair) => pair,
            Err(err) => {
                let step_name = "<pre-flight>".to_string();
                if run.step_started(step_name.clone()).did_execute() {
                    self.runs.update(&mut run).await?;
                }
                if run.step_errored(step_name, err.to_string()).did_execute() {
                    self.runs.update(&mut run).await?;
                }
                if run.run_completed().did_execute() {
                    self.runs.update(&mut run).await?;
                }
                self.suspend_workflow_sandboxes(project_id, workflow_id, &HashSet::new())
                    .await;
                return Ok(());
            }
        };

        // Preexisting sandboxes the workflow successfully attached to.
        // Drives the post-flight suspend decision: a Preexisting sandbox
        // is suspended at end-of-run only if (a) we attached at least
        // once during the run AND (b) no other agent (workflow or user)
        // is still attached at post-flight time.
        let mut borrowed_preexisting: HashSet<SandboxId> = HashSet::new();

        for step in &steps {
            let step_name = step.name().to_string();

            if run.step_already_terminal(&step_name) {
                continue;
            }

            if cancel.load(Ordering::Relaxed) {
                return Err(WorkflowError::Cancelled);
            }

            // Condition gate. Evaluated BEFORE `step_started`: a
            // skipped step never enters Running state — it's
            // terminally Skipped from the start. `evaluate_condition`
            // folds parse + execute into one call; create-time
            // validation guarantees the body compiles, so an `Err`
            // here is a CEL runtime error (type mismatch, etc.).
            if let Some(body) = step.condition() {
                let step_outputs = collect_step_outputs(&run.step_results);
                let ctx = TemplateContext {
                    trigger: &trigger_context,
                    steps: &step_outputs,
                };
                match ctx.evaluate_condition(body) {
                    Ok(ConditionOutcome::True) => {
                        // Fall through to step_started + execute_step.
                    }
                    Ok(ConditionOutcome::False) => {
                        if run.step_skipped(step_name, body.to_string()).did_execute() {
                            self.runs.update(&mut run).await?;
                        }
                        continue;
                    }
                    Ok(ConditionOutcome::NotBoolean(rendered)) => {
                        let err = WorkflowError::ConditionNotBoolean {
                            step: step_name.clone(),
                            body: body.to_string(),
                            value: rendered,
                        };
                        if run.step_errored(step_name, err.to_string()).did_execute() {
                            self.runs.update(&mut run).await?;
                        }
                        break;
                    }
                    Err(e) => {
                        let err =
                            WorkflowError::InvalidCondition(format!("step '{step_name}': {e}"));
                        if run.step_errored(step_name, err.to_string()).did_execute() {
                            self.runs.update(&mut run).await?;
                        }
                        break;
                    }
                }
            }

            if let WorkflowStepDef::Wait { provider, .. } = step {
                if run
                    .step_waiting(step_name.clone(), provider.clone())
                    .did_execute()
                {
                    self.runs.update(&mut run).await?;
                }
                self.suspend_workflow_sandboxes(project_id, workflow_id, &borrowed_preexisting)
                    .await;
                return Ok(());
            }

            // Loop step: keep entity bookkeeping (outer step_started
            // here, step_completed below) identical to the other
            // variants, but defer the inner iteration logic to
            // `run_loop_step`. `LoopOutcome::Parked` short-circuits
            // the outer loop the same way a Wait at the outer level
            // does.
            if let WorkflowStepDef::Loop { .. } = step {
                if run.step_started(step_name.clone()).did_execute() {
                    self.runs.update(&mut run).await?;
                }
                match self
                    .run_loop_step(
                        project_id,
                        workflow_id,
                        run_id,
                        step,
                        &trigger_context,
                        &sandbox_ids,
                        &preexisting_ids,
                        &mut borrowed_preexisting,
                        &definition,
                        &mut run,
                        &cancel,
                    )
                    .await
                {
                    Ok(LoopOutcome::Completed(output)) => {
                        if run.step_completed(step_name, output).did_execute() {
                            self.runs.update(&mut run).await?;
                        }
                        continue;
                    }
                    Ok(LoopOutcome::Parked) => {
                        self.suspend_workflow_sandboxes(
                            project_id,
                            workflow_id,
                            &borrowed_preexisting,
                        )
                        .await;
                        return Ok(());
                    }
                    Ok(LoopOutcome::Cancelled) => {
                        return Err(WorkflowError::Cancelled);
                    }
                    Err(err) => {
                        if run.step_errored(step_name, err.to_string()).did_execute() {
                            self.runs.update(&mut run).await?;
                        }
                        break;
                    }
                }
            }

            if run.step_started(step_name.clone()).did_execute() {
                self.runs.update(&mut run).await?;
            }

            let step_outputs = collect_step_outputs(&run.step_results);
            let outcome = self
                .execute_step(
                    project_id,
                    workflow_id,
                    run_id,
                    step,
                    &trigger_context,
                    &step_outputs,
                    &sandbox_ids,
                    &preexisting_ids,
                    &mut borrowed_preexisting,
                    &definition,
                )
                .await;

            match outcome {
                Ok(output) => {
                    if run.step_completed(step_name, output).did_execute() {
                        self.runs.update(&mut run).await?;
                    }
                }
                Err(err) => {
                    if run.step_errored(step_name, err.to_string()).did_execute() {
                        self.runs.update(&mut run).await?;
                    }
                    break;
                }
            }
        }

        if run.run_completed().did_execute() {
            self.runs.update(&mut run).await?;
        }

        // Post-flight: always runs (even when a step failed).
        // Best-effort. Workflow-scoped sandboxes always suspend; borrowed
        // preexisting sandboxes only suspend if uncontested.
        self.suspend_workflow_sandboxes(project_id, workflow_id, &borrowed_preexisting)
            .await;

        Ok(())
    }

    /// For each declared sandbox: find or create scoped to the workflow,
    /// restart if Suspended, then wait until Ready. Returns a name → id
    /// map plus the set of Preexisting sandbox ids (used by post-flight
    /// to apply the borrowed-and-uncontested suspend rule).
    async fn ensure_sandboxes_ready(
        &self,
        project_id: ProjectId,
        workflow_id: WorkflowDefinitionId,
        decls: &[WorkflowSandboxDecl],
    ) -> Result<(HashMap<String, SandboxId>, HashSet<SandboxId>), WorkflowError> {
        let mut ids: HashMap<String, SandboxId> = HashMap::with_capacity(decls.len());
        let mut preexisting_ids: HashSet<SandboxId> = HashSet::new();
        for decl in decls {
            let (decl_name, sandbox) = match decl {
                // Reference an already-existing sandbox in the project
                // by its (unique) name. Auto-restart if Suspended or
                // Errored — borrowed sandboxes share the workflow-scoped
                // wake-up lifecycle: the user might have suspended it
                // explicitly or via a prior workflow's post-flight, and
                // the next trigger should bring it back to Ready.
                WorkflowSandboxDecl::Preexisting { name } => {
                    let sb = self
                        .sandboxes
                        .find_by_name_in_project_unchecked(project_id, name)
                        .await
                        .map_err(|e| {
                            WorkflowError::SandboxNotFound(format!(
                                "preexisting sandbox '{name}': {e}"
                            ))
                        })?;
                    let sb = self.restart_if_dormant(sb, name).await?;
                    (name, sb)
                }
                // Workflow-scoped sandbox: find / create / restart.
                WorkflowSandboxDecl::Provisioned { name, mode, specs } => {
                    let existing = self
                        .sandboxes
                        .find_for_workflow(project_id, workflow_id, name)
                        .await?;

                    let sandbox = match existing {
                        None => {
                            let specs = specs.clone().unwrap_or_else(|| SandboxSpecs {
                                cpu: "500m".to_string(),
                                memory: "512Mi".to_string(),
                                disk_size: "10Gi".to_string(),
                            });
                            self.sandboxes
                                .create_for_workflow(
                                    project_id,
                                    workflow_id,
                                    name.clone(),
                                    specs,
                                    mode.clone(),
                                )
                                .await?
                        }
                        Some(sb) => self.restart_if_dormant(sb, name).await?,
                    };
                    (name, sandbox)
                }
            };

            self.sandboxes
                .wait_until_ready(sandbox.id, SANDBOX_READY_TIMEOUT)
                .await
                .map_err(|e| WorkflowError::SandboxNotReady {
                    name: decl_name.clone(),
                    state: e.to_string(),
                })?;
            if matches!(decl, WorkflowSandboxDecl::Preexisting { .. }) {
                preexisting_ids.insert(sandbox.id);
            }
            ids.insert(decl_name.clone(), sandbox.id);
        }
        Ok((ids, preexisting_ids))
    }

    /// Wakes a Suspended or Errored sandbox so the workflow can attach.
    /// Errored is logged at warn so the prior failure stays visible in
    /// observability.
    async fn restart_if_dormant(
        &self,
        sandbox: Sandbox,
        decl_name: &str,
    ) -> Result<Sandbox, WorkflowError> {
        if !matches!(
            sandbox.state,
            SandboxState::Suspended | SandboxState::Errored
        ) {
            return Ok(sandbox);
        }
        if sandbox.state == SandboxState::Errored {
            tracing::warn!(
                sandbox_id = %sandbox.id,
                sandbox_name = %decl_name,
                last_error = ?sandbox.last_error,
                "pre-flight: restarting sandbox in Errored state",
            );
        }
        Ok(self.sandboxes.restart_for_workflow(sandbox.id).await?)
    }

    async fn suspend_workflow_sandboxes(
        &self,
        project_id: ProjectId,
        workflow_id: WorkflowDefinitionId,
        borrowed_preexisting: &HashSet<SandboxId>,
    ) {
        let definition = match self.definitions.find_by_id(workflow_id).await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = %e, "post-flight: failed to load definition; skipping suspend");
                return;
            }
        };
        let mut op = match self.sandboxes.begin_op().await {
            Ok(op) => op,
            Err(e) => {
                tracing::warn!(error = %e, "post-flight: begin_op failed; skipping suspend");
                return;
            }
        };
        for decl in &definition.sandboxes {
            let (name, sandbox) = match decl {
                // Workflow-private; always suspend.
                WorkflowSandboxDecl::Provisioned { name, .. } => {
                    match self
                        .sandboxes
                        .find_for_workflow(project_id, workflow_id, name)
                        .await
                    {
                        Ok(Some(sb)) => (name, sb),
                        Ok(None) => continue,
                        Err(e) => {
                            tracing::warn!(error = %e, sandbox = %name, "post-flight: lookup failed");
                            continue;
                        }
                    }
                }
                // Borrowed-and-uncontested: suspend iff (a) the workflow
                // attached to this sandbox at least once during the run
                // AND (b) no other agent (workflow or user) is currently
                // attached. Whichever run is the last one out flips it
                // to Suspended.
                WorkflowSandboxDecl::Preexisting { name } => {
                    let sb = match self
                        .sandboxes
                        .find_by_name_in_project_unchecked(project_id, name)
                        .await
                    {
                        Ok(sb) => sb,
                        Err(e) => {
                            tracing::warn!(error = %e, sandbox = %name, "post-flight: lookup failed");
                            continue;
                        }
                    };
                    if !borrowed_preexisting.contains(&sb.id) {
                        continue;
                    }
                    if !sb.attached_agents.is_empty() {
                        tracing::info!(
                            sandbox_id = %sb.id,
                            sandbox = %name,
                            still_attached = sb.attached_agents.len(),
                            "post-flight: deferring suspend; other agents still attached",
                        );
                        continue;
                    }
                    (name, sb)
                }
            };
            if sandbox.state == SandboxState::Suspended {
                continue;
            }
            if let Err(e) = self.sandboxes.suspend_in_op(&mut op, sandbox.id).await {
                tracing::warn!(
                    error = %e,
                    sandbox_id = %sandbox.id,
                    sandbox = %name,
                    "post-flight: suspend failed",
                );
            }
        }
        if let Err(e) = op.commit().await {
            tracing::warn!(error = %e, "post-flight: suspend commit failed");
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_step(
        &self,
        project_id: ProjectId,
        workflow_id: WorkflowDefinitionId,
        run_id: WorkflowRunId,
        step: &WorkflowStepDef,
        trigger_context: &serde_json::Value,
        step_outputs: &HashMap<String, serde_json::Value>,
        sandbox_ids: &HashMap<String, SandboxId>,
        preexisting_ids: &HashSet<SandboxId>,
        borrowed_preexisting: &mut HashSet<SandboxId>,
        definition: &super::entity::WorkflowDefinition,
    ) -> Result<serde_json::Value, WorkflowError> {
        match step {
            WorkflowStepDef::AgentStep {
                name,
                skill,
                sandbox,
                sandbox_mode,
                timeout_seconds,
                output_schema,
                ..
            } => {
                // Two-pass interpolation, ORDER MATTERS for security:
                //
                //   1. Resolve `${{ trigger.X }}` / `${{ steps.<n>.outputs.Y }}`
                //      against the raw skill body. The skill author's
                //      template references resolve here.
                //   2. Then splice in the JSON-serialised trigger payload as
                //      OPAQUE text. A literal `${{ … }}` in user-controlled
                //      payload content (a commit message, a webhook field, …)
                //      survives this expansion but is no longer re-evaluated,
                //      so an attacker cannot leak prior step outputs into the
                //      prompt by smuggling templates through the trigger.
                let sandbox_id = sandbox.as_deref().and_then(|n| sandbox_ids.get(n).copied());
                let raw_body: String = self
                    .skills
                    .find_by_name(skill, Some(project_id), sandbox_id)
                    .await
                    .map_err(|e| WorkflowError::Skill(e.to_string()))?
                    .ok_or_else(|| WorkflowError::SkillNotFound(skill.clone()))?
                    .into();
                let template_ctx = TemplateContext {
                    trigger: trigger_context,
                    steps: step_outputs,
                };
                let templated_body = template_ctx
                    .substitute_in_string(&raw_body)
                    .map_err(|e| WorkflowError::Skill(e.to_string()))?;
                let prompt = build_agent_prompt(templated_body, trigger_context);

                self.dispatch_agent_step(
                    project_id,
                    workflow_id,
                    run_id,
                    name,
                    sandbox.as_deref(),
                    sandbox_mode.unwrap_or(SandboxAgentMode::Write),
                    *timeout_seconds,
                    output_schema.as_ref(),
                    step,
                    prompt,
                    sandbox_ids,
                    preexisting_ids,
                    borrowed_preexisting,
                    definition,
                )
                .await
            }
            WorkflowStepDef::ToolStep {
                name,
                tool,
                params,
                timeout_seconds,
                ..
            } => {
                self.execute_tool_step(
                    project_id,
                    workflow_id,
                    run_id,
                    name,
                    tool,
                    params,
                    *timeout_seconds,
                    trigger_context,
                    step_outputs,
                )
                .await
            }
            // Wait steps are handled in the main loop before reaching
            // execute_step; this arm should be unreachable.
            WorkflowStepDef::Wait { name, .. } => Err(WorkflowError::StepErrored {
                step: name.clone(),
                reason: "wait step reached execute_step unexpectedly".into(),
            }),
            // Loop steps are dispatched via `run_loop_step` directly
            // from the outer loop in `run`; this arm should be
            // unreachable.
            WorkflowStepDef::Loop { name, .. } => Err(WorkflowError::StepErrored {
                step: name.clone(),
                reason: "loop step reached execute_step unexpectedly".into(),
            }),
        }
    }

    /// Post-prompt dispatch: open the agent, attach the sandbox (if
    /// any), run the chat session to `submit_output`, detach. Shared
    /// by both the top-level `AgentStep` arm and the inner-loop
    /// `AgentStep` path so the sandbox/agent lifecycle stays
    /// identical regardless of where the step lives. `step` is the
    /// inner [`WorkflowStepDef::AgentStep`] used only to resolve the
    /// model chain override.
    #[allow(clippy::too_many_arguments)]
    async fn dispatch_agent_step(
        &self,
        project_id: ProjectId,
        workflow_id: WorkflowDefinitionId,
        run_id: WorkflowRunId,
        step_name: &str,
        sandbox: Option<&str>,
        sandbox_mode: SandboxAgentMode,
        timeout_seconds: Option<u64>,
        output_schema: &OutputSchema,
        chain_step: &WorkflowStepDef,
        prompt: String,
        sandbox_ids: &HashMap<String, SandboxId>,
        preexisting_ids: &HashSet<SandboxId>,
        borrowed_preexisting: &mut HashSet<SandboxId>,
        definition: &super::entity::WorkflowDefinition,
    ) -> Result<serde_json::Value, WorkflowError> {
        let attach_sandbox = match sandbox {
            Some(sandbox_name) => {
                let id = sandbox_ids
                    .get(sandbox_name)
                    .copied()
                    .ok_or_else(|| WorkflowError::UndeclaredSandbox(sandbox_name.to_string()))?;
                Some((id, sandbox_mode))
            }
            None => None,
        };

        let agent_name = format!("workflow-{}-{step_name}", run_id.short());
        let mut op = self.agents.begin_op().await?;
        let existing = self
            .agents
            .find_for_workflow_step_in_op(&mut op, run_id, &agent_name)
            .await?;

        let (agent, detached_agents) = if let Some(agent) = existing {
            (agent, Vec::new())
        } else {
            let detached_agents = match attach_sandbox {
                Some((sandbox_id, mode)) => {
                    self.agents
                        .detach_conflicting_writer_in_op(
                            &mut op,
                            sandbox_id,
                            mode,
                            Some(workflow_id),
                        )
                        .await?
                }
                None => Vec::new(),
            };
            let chain_override = definition.resolve_step_chain(chain_step);
            let agent = self
                .agents
                .create_for_workflow_run_in_op(
                    &mut op,
                    project_id,
                    workflow_id,
                    run_id,
                    &agent_name,
                    attach_sandbox,
                    chain_override,
                    output_schema.clone(),
                )
                .await?;
            (agent, detached_agents)
        };
        op.commit().await?;

        if let Some((sandbox_id, _)) = attach_sandbox {
            if preexisting_ids.contains(&sandbox_id) {
                borrowed_preexisting.insert(sandbox_id);
            }
        }
        for prior in detached_agents {
            self.agents.invalidate_agent_cache(prior);
        }

        // Reset the workspace repo so leftover state from a prior
        // workflow run (stale `bot/*` branches, dirty checkout)
        // doesn't leak into this agent's view of HEAD.
        if let Some((sandbox_id, _)) = attach_sandbox {
            self.sandboxes.reset_for_workflow_step(sandbox_id).await;
        }

        let result = self
            .run_agent_until_submit_output(&agent, prompt, step_name, timeout_seconds)
            .await;

        // Detach the sandbox unconditionally so the next step can
        // attach (Write mode is single-writer). Best-effort.
        if let Some((sandbox_id, _)) = attach_sandbox {
            self.detach_step_sandbox(sandbox_id, agent.id).await;
        }
        result
    }

    /// Drive a `WorkflowStepDef::Loop` until its `break_condition`
    /// fires or `max_iterations` is exhausted. Each iteration's inner
    /// steps are recorded as dotted-name `StepResult` rows
    /// (`{loop_name}.iter[{N}].{inner_name}`), so the existing
    /// idempotency (`step_already_terminal`) and resume-from-wait
    /// (`current_wait_step`) infrastructure keeps working unchanged.
    /// Resumable: replays of an in-flight iteration pick up where
    /// the prior attempt left off via [`WorkflowRun::loop_current_iteration`]
    /// and per-inner-step terminal checks.
    #[allow(clippy::too_many_arguments)]
    async fn run_loop_step(
        &self,
        project_id: ProjectId,
        workflow_id: WorkflowDefinitionId,
        run_id: WorkflowRunId,
        loop_step: &WorkflowStepDef,
        trigger_context: &serde_json::Value,
        sandbox_ids: &HashMap<String, SandboxId>,
        preexisting_ids: &HashSet<SandboxId>,
        borrowed_preexisting: &mut HashSet<SandboxId>,
        definition: &super::entity::WorkflowDefinition,
        run: &mut WorkflowRun,
        cancel: &Arc<AtomicBool>,
    ) -> Result<LoopOutcome, WorkflowError> {
        let (loop_name, break_condition, max_iterations, inner_steps) = match loop_step {
            WorkflowStepDef::Loop {
                name,
                break_condition,
                max_iterations,
                steps,
                ..
            } => (
                name.clone(),
                break_condition.clone(),
                *max_iterations,
                steps,
            ),
            _ => unreachable!("run_loop_step called with non-Loop step"),
        };

        let mut iteration: u32 = run.loop_current_iteration(&loop_name).max(1);

        loop {
            if iteration > max_iterations {
                let iters = run.loop_iterations_so_far(&loop_name);
                return Ok(LoopOutcome::Completed(serde_json::json!({
                    "success": false,
                    "reason": format!(
                        "loop '{loop_name}' reached max_iterations ({max_iterations}) without break_condition firing"
                    ),
                    "iterations": iters,
                })));
            }

            if cancel.load(Ordering::Relaxed) {
                return Ok(LoopOutcome::Cancelled);
            }

            if run
                .loop_iteration_started(loop_name.clone(), iteration)
                .did_execute()
            {
                self.runs.update(run).await?;
            }

            // Resume safety: if a prior attempt recorded broke=true
            // we exit here instead of running another iteration.
            if run.loop_broke(&loop_name) {
                let iters = run.loop_iterations_so_far(&loop_name);
                return Ok(LoopOutcome::Completed(serde_json::json!({
                    "success": true,
                    "iterations": iters,
                })));
            }

            for inner in inner_steps {
                let dotted_name = format!("{loop_name}.iter[{iteration}].{}", inner.name());
                if run.step_already_terminal(&dotted_name) {
                    continue;
                }
                if cancel.load(Ordering::Relaxed) {
                    return Ok(LoopOutcome::Cancelled);
                }

                let outer_step_outputs = collect_step_outputs(&run.step_results);
                let inner_outputs =
                    collect_inner_iter_outputs(&run.step_results, &loop_name, iteration);
                let ictx = IterationContext {
                    trigger: trigger_context,
                    steps: &outer_step_outputs,
                    iter: &inner_outputs,
                    iteration,
                };

                if let Some(body) = inner.condition() {
                    match ictx.evaluate_condition(body) {
                        Ok(ConditionOutcome::True) => {}
                        Ok(ConditionOutcome::False) => {
                            if run
                                .step_skipped(dotted_name.clone(), body.to_string())
                                .did_execute()
                            {
                                self.runs.update(run).await?;
                            }
                            continue;
                        }
                        Ok(ConditionOutcome::NotBoolean(rendered)) => {
                            return Err(WorkflowError::ConditionNotBoolean {
                                step: dotted_name,
                                body: body.to_string(),
                                value: rendered,
                            });
                        }
                        Err(e) => {
                            return Err(WorkflowError::InvalidCondition(format!(
                                "step '{dotted_name}': {e}"
                            )));
                        }
                    }
                }

                if let WorkflowStepDef::Wait { provider, .. } = inner {
                    if run
                        .step_waiting(dotted_name.clone(), provider.clone())
                        .did_execute()
                    {
                        self.runs.update(run).await?;
                    }
                    return Ok(LoopOutcome::Parked);
                }

                if run.step_started(dotted_name.clone()).did_execute() {
                    self.runs.update(run).await?;
                }

                let result = match inner {
                    WorkflowStepDef::AgentStep {
                        skill,
                        sandbox,
                        sandbox_mode,
                        timeout_seconds,
                        output_schema,
                        ..
                    } => {
                        let sandbox_id =
                            sandbox.as_deref().and_then(|n| sandbox_ids.get(n).copied());
                        let raw_body: String = match self
                            .skills
                            .find_by_name(skill, Some(project_id), sandbox_id)
                            .await
                        {
                            Ok(Some(s)) => s.into(),
                            Ok(None) => {
                                let err = WorkflowError::SkillNotFound(skill.clone());
                                if run
                                    .step_errored(dotted_name.clone(), err.to_string())
                                    .did_execute()
                                {
                                    self.runs.update(run).await?;
                                }
                                return Err(err);
                            }
                            Err(e) => {
                                let err = WorkflowError::Skill(e.to_string());
                                if run
                                    .step_errored(dotted_name.clone(), err.to_string())
                                    .did_execute()
                                {
                                    self.runs.update(run).await?;
                                }
                                return Err(err);
                            }
                        };
                        let templated_body = match ictx.substitute_in_string(&raw_body) {
                            Ok(s) => s,
                            Err(e) => {
                                let err = WorkflowError::Skill(e.to_string());
                                if run
                                    .step_errored(dotted_name.clone(), err.to_string())
                                    .did_execute()
                                {
                                    self.runs.update(run).await?;
                                }
                                return Err(err);
                            }
                        };
                        let prompt = build_agent_prompt(templated_body, trigger_context);
                        self.dispatch_agent_step(
                            project_id,
                            workflow_id,
                            run_id,
                            &dotted_name,
                            sandbox.as_deref(),
                            sandbox_mode.unwrap_or(SandboxAgentMode::Write),
                            *timeout_seconds,
                            output_schema.as_ref(),
                            inner,
                            prompt,
                            sandbox_ids,
                            preexisting_ids,
                            borrowed_preexisting,
                            definition,
                        )
                        .await
                    }
                    WorkflowStepDef::ToolStep {
                        tool,
                        params,
                        timeout_seconds,
                        ..
                    } => {
                        // Pre-resolve `${{ … }}` refs via the iteration
                        // context (covers trigger/steps/iter/iteration).
                        // Then call `execute_tool_step` with empty
                        // trigger/steps maps so its own substitution
                        // pass is a no-op on the already-resolved tree.
                        let resolved = match ictx.substitute(params) {
                            Ok(v) => v,
                            Err(e) => {
                                let err = WorkflowError::InvalidTemplateRef(e.to_string());
                                if run
                                    .step_errored(dotted_name.clone(), err.to_string())
                                    .did_execute()
                                {
                                    self.runs.update(run).await?;
                                }
                                return Err(err);
                            }
                        };
                        let empty_outputs: HashMap<String, serde_json::Value> = HashMap::new();
                        let empty_trigger = serde_json::Value::Null;
                        self.execute_tool_step(
                            project_id,
                            workflow_id,
                            run_id,
                            &dotted_name,
                            tool,
                            &resolved,
                            *timeout_seconds,
                            &empty_trigger,
                            &empty_outputs,
                        )
                        .await
                    }
                    WorkflowStepDef::Wait { .. } | WorkflowStepDef::Loop { .. } => unreachable!(
                        "Wait handled via park-and-return; nested Loop rejected at validate time"
                    ),
                };

                match result {
                    Ok(output) => {
                        if run.step_completed(dotted_name, output).did_execute() {
                            self.runs.update(run).await?;
                        }
                    }
                    Err(err) => {
                        if run.step_errored(dotted_name, err.to_string()).did_execute() {
                            self.runs.update(run).await?;
                        }
                        return Err(err);
                    }
                }
            }

            // All inner steps for this iteration complete. Evaluate
            // break_condition against the now-finalized iter map.
            let outer_step_outputs = collect_step_outputs(&run.step_results);
            let inner_outputs =
                collect_inner_iter_outputs(&run.step_results, &loop_name, iteration);
            let ictx = IterationContext {
                trigger: trigger_context,
                steps: &outer_step_outputs,
                iter: &inner_outputs,
                iteration,
            };
            let broke = match ictx.evaluate_condition(&break_condition) {
                Ok(ConditionOutcome::True) => true,
                Ok(ConditionOutcome::False) => false,
                Ok(ConditionOutcome::NotBoolean(rendered)) => {
                    return Err(WorkflowError::ConditionNotBoolean {
                        step: loop_name.clone(),
                        body: break_condition.clone(),
                        value: rendered,
                    });
                }
                Err(e) => {
                    return Err(WorkflowError::InvalidCondition(format!(
                        "loop '{loop_name}' break_condition: {e}"
                    )));
                }
            };

            if run
                .loop_iteration_completed(loop_name.clone(), iteration, broke)
                .did_execute()
            {
                self.runs.update(run).await?;
            }

            if broke {
                let iters = run.loop_iterations_so_far(&loop_name);
                return Ok(LoopOutcome::Completed(serde_json::json!({
                    "success": true,
                    "iterations": iters,
                })));
            }

            iteration += 1;
        }
    }

    /// First the regular send/resume loop; if the agent finished without
    /// calling `submit_output`, retry once with `tool_choice` forced to
    /// the synthetic tool. After a second miss the step fails.
    async fn run_agent_until_submit_output(
        &self,
        agent: &Agent,
        prompt: String,
        step_name: &str,
        timeout_seconds: Option<u64>,
    ) -> Result<serde_json::Value, WorkflowError> {
        if let Some(value) = self
            .stream_agent_response(agent, Some(prompt), None, step_name, timeout_seconds)
            .await?
        {
            return Ok(value);
        }

        // Forced retry: the agent ended its turn without calling
        // `submit_output`. Nudge it with a fresh user message and
        // `tool_choice: Tool { submit_output }` so the next assistant
        // turn is constrained to the synthetic tool.
        let nudge = "Investigation complete — call `submit_output` now to record the structured \
             result for this step.";
        if let Some(value) = self
            .stream_agent_response(
                agent,
                Some(nudge.to_string()),
                Some(llm::prompt::ToolChoice::Tool {
                    name: SUBMIT_OUTPUT_TOOL_NAME.to_string(),
                }),
                step_name,
                timeout_seconds,
            )
            .await?
        {
            return Ok(value);
        }

        Err(WorkflowError::StepErrored {
            step: step_name.to_string(),
            reason: "agent did not call submit_output after one forced retry".to_string(),
        })
    }

    /// Drives one send/resume turn to completion. Returns the
    /// structured output if the session reached `Done` via a
    /// `submit_output` call (read from the persisted session event).
    /// `None` means the turn closed without `submit_output` — the
    /// caller decides whether to retry.
    async fn stream_agent_response(
        &self,
        agent: &Agent,
        user_prompt: Option<String>,
        tool_choice: Option<llm::prompt::ToolChoice>,
        step_name: &str,
        timeout_seconds: Option<u64>,
    ) -> Result<Option<serde_json::Value>, WorkflowError> {
        let agent_subject = agent.auth_subject();
        // Resume an in-flight prompt if a prior attempt was killed between
        // `PromptSent` and `AssistantResponseReceived`. Otherwise emit the
        // step's prompt as a fresh user input. `pending_prompt` returns
        // `None` whenever the last response-related event is
        // `AssistantResponseReceived` (success or recorded error), so a
        // closed turn flows through to `send_message` as expected.
        let mut rx = match self
            .agents
            .resume_message(agent_subject.clone(), agent.id)
            .await?
        {
            Some(rx) => rx,
            None => match user_prompt {
                Some(prompt) => {
                    self.agents
                        .send_message_with_choice(agent_subject, agent.id, prompt, tool_choice)
                        .await?
                }
                None => return Ok(None),
            },
        };

        // Idle timeout: resets on every streamed event so a busy agent
        // can run indefinitely. Trips only when nothing arrives for the
        // configured window.
        let idle_timeout = timeout_seconds
            .map(Duration::from_secs)
            .unwrap_or_else(|| Duration::from_secs(300));

        loop {
            let event = match tokio::time::timeout(idle_timeout, rx.recv()).await {
                Ok(Some(event)) => event,
                Ok(None) => break,
                Err(_) => {
                    return Err(WorkflowError::StepErrored {
                        step: step_name.to_string(),
                        reason: format!("idle for {}s", idle_timeout.as_secs()),
                    });
                }
            };
            match event {
                ChatOutputEvent::AssistantDone { .. } => break,
                ChatOutputEvent::Error { message } => {
                    return Err(WorkflowError::StepErrored {
                        step: step_name.to_string(),
                        reason: message,
                    });
                }
                _ => {}
            }
        }

        // The session is the source of truth: `add_tool_results`
        // pushed `OutputSubmitted` and returned `Done` when the agent
        // called `submit_output`, which broke the agent loop and
        // emitted `AssistantDone`. Read it back here.
        Ok(self.agents.submitted_output(agent.id).await?)
    }

    /// Dispatch a single top-level MCP tool with `${{ … }}`-substituted
    /// params. Mints an `AuthSubject::WorkflowExecutor` carrying the
    /// workflow's project authority. The called tool MUST declare
    /// `output_schema()` (Q9-c, memo `019e01a4`); the resulting
    /// `structured_content` is what flows into `StepResult.output`.
    /// Tool errors (`is_error: true` or absent structured content)
    /// fail the step.
    #[allow(clippy::too_many_arguments)]
    async fn execute_tool_step(
        &self,
        project_id: ProjectId,
        workflow_id: WorkflowDefinitionId,
        run_id: WorkflowRunId,
        step_name: &str,
        tool_name: &str,
        params: &serde_json::Value,
        timeout_seconds: Option<u64>,
        trigger_context: &serde_json::Value,
        step_outputs: &HashMap<String, serde_json::Value>,
    ) -> Result<serde_json::Value, WorkflowError> {
        // Workflow contract is enforced by `find_for_workflow`
        // (registered + composable + declares output_schema). The
        // same check runs at workflow-create / update time in
        // `Workflows::validate_steps`, so this branch is defensive
        // — top-level tool registration is build-time only.
        self.toolsets
            .find_for_workflow(tool_name)
            .map_err(|e| WorkflowError::ToolNotFound(format!("tool_step '{step_name}': {e}")))?;

        let template_ctx = TemplateContext {
            trigger: trigger_context,
            steps: step_outputs,
        };
        let resolved_params = template_ctx
            .substitute(params)
            .map_err(|e| WorkflowError::InvalidTemplateRef(e.to_string()))?;
        // Walk the params/resolved trees once up front so the
        // happy path drops `resolved_params` cleanly into the
        // dispatch arguments below; the rejection paths share the
        // same `diagnostics` slice and only pay the formatting
        // cost when they fire.
        let diagnostics = template_diagnostics(params, &resolved_params);
        let diagnose = || format_template_diagnostics(&diagnostics);

        // Top-level tools accept `Option<JsonObject>`. Coerce non-objects
        // to an empty object — the tool's input schema rejects the
        // missing-field case with a clear error if it cared.
        let arguments = match resolved_params {
            serde_json::Value::Object(map) => Some(map),
            serde_json::Value::Null => None,
            other => {
                return Err(WorkflowError::InvalidStep(format!(
                    "tool_step '{step_name}': resolved params must be a JSON object, got {}",
                    type_label(&other)
                )));
            }
        };

        let subject = AuthSubject::workflow_executor(project_id, workflow_id, run_id);
        let timeout =
            Duration::from_secs(timeout_seconds.unwrap_or(DEFAULT_TOOL_STEP_TIMEOUT_SECS));

        let call = self
            .toolsets
            .call_top_level_tool(&subject, tool_name, arguments);
        let result = match tokio::time::timeout(timeout, call).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                return Err(WorkflowError::ToolDispatch(format!(
                    "tool '{tool_name}': {e}{}",
                    diagnose()
                )));
            }
            Err(_) => {
                return Err(WorkflowError::StepErrored {
                    step: step_name.to_string(),
                    reason: format!("tool '{tool_name}' timed out after {}s", timeout.as_secs()),
                })
            }
        };

        if result.is_error.unwrap_or(false) {
            let detail = first_text_content(&result)
                .unwrap_or_else(|| "tool returned is_error: true with no text content".to_string());
            return Err(WorkflowError::StepErrored {
                step: step_name.to_string(),
                reason: format!("tool '{tool_name}' failed: {detail}{}", diagnose()),
            });
        }

        result
            .structured_content
            .ok_or_else(|| WorkflowError::StepErrored {
                step: step_name.to_string(),
                reason: format!(
                    "tool '{tool_name}' returned no structured_content; tool_step requires it"
                ),
            })
    }

    async fn detach_step_sandbox(&self, sandbox_id: SandboxId, agent_id: AgentId) {
        let mut op = match self.sandboxes.begin_op().await {
            Ok(op) => op,
            Err(e) => {
                tracing::warn!(error = %e, "step cleanup: begin_op failed; sandbox left attached");
                return;
            }
        };
        if let Err(e) = self
            .sandboxes
            .detach_from_agent_in_op(&mut op, sandbox_id, agent_id)
            .await
        {
            tracing::warn!(error = %e, %sandbox_id, %agent_id, "step cleanup: detach failed");
            return;
        }
        if let Err(e) = op.commit().await {
            tracing::warn!(error = %e, "step cleanup: detach commit failed");
        }
    }
}
