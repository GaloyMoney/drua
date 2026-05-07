use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::agent::session::message::SUBMIT_OUTPUT_TOOL_NAME;
use crate::agent::{Agent, Agents};
use crate::primitives::{
    AgentId, ChatOutputEvent, ProjectId, SandboxId, WorkflowDefinitionId, WorkflowRunId,
};
use crate::sandbox::{Sandbox, SandboxAgentMode, SandboxSpecs, SandboxState, Sandboxes};
use crate::skill::Skills;

use super::definition::{WorkflowSandboxDecl, WorkflowStepDef};
use super::error::WorkflowError;
use super::repo::WorkflowDefinitionRepo;
use super::run::{WorkflowRunRepo, WorkflowRunState};

/// Hard cap on the pre-flight per-sandbox readiness wait. Protects the
/// queue from a stuck infrastructure step.
const SANDBOX_READY_TIMEOUT: Duration = Duration::from_secs(180);

pub struct Executor {
    runs: WorkflowRunRepo,
    definitions: WorkflowDefinitionRepo,
    agents: Arc<Agents>,
    skills: Arc<Skills>,
    sandboxes: Arc<Sandboxes>,
}

impl Executor {
    pub fn new(
        runs: WorkflowRunRepo,
        definitions: WorkflowDefinitionRepo,
        agents: Arc<Agents>,
        skills: Arc<Skills>,
        sandboxes: Arc<Sandboxes>,
    ) -> Self {
        Self {
            runs,
            definitions,
            agents,
            skills,
            sandboxes,
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

            if run.step_started(step_name.clone()).did_execute() {
                self.runs.update(&mut run).await?;
            }

            let outcome = self
                .execute_step(
                    project_id,
                    workflow_id,
                    run_id,
                    step,
                    &trigger_context,
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
                ..
            } => {
                let attach_sandbox = match sandbox.as_deref() {
                    Some(sandbox_name) => {
                        let id = sandbox_ids.get(sandbox_name).copied().ok_or_else(|| {
                            WorkflowError::UndeclaredSandbox(sandbox_name.to_string())
                        })?;
                        Some((id, sandbox_mode.unwrap_or(SandboxAgentMode::Write)))
                    }
                    None => None,
                };

                let arguments = serde_json::to_string_pretty(trigger_context)
                    .unwrap_or_else(|_| trigger_context.to_string());
                let sandbox_id = attach_sandbox.map(|(id, _)| id);
                let prompt = self
                    .skills
                    .interpolate_skill(skill, Some(project_id), sandbox_id, Some(&arguments))
                    .await
                    .map_err(|e| WorkflowError::Skill(e.to_string()))?
                    .ok_or_else(|| WorkflowError::SkillNotFound(skill.clone()))?;

                let agent_name = format!("workflow-{}-{name}", run_id.short());
                let mut op = self
                    .agents
                    .begin_op()
                    .await
                    .map_err(|e| WorkflowError::Agent(e.to_string()))?;

                let existing = self
                    .agents
                    .find_for_workflow_step_in_op(&mut op, run_id, &agent_name)
                    .await
                    .map_err(|e| WorkflowError::Agent(e.to_string()))?;

                let (agent, detached_agents) = if let Some(agent) = existing {
                    (agent, Vec::new())
                } else {
                    let detached_agents = match attach_sandbox {
                        Some((sandbox_id, mode)) => self
                            .agents
                            .detach_conflicting_writer_in_op(
                                &mut op,
                                sandbox_id,
                                mode,
                                Some(workflow_id),
                            )
                            .await
                            .map_err(|e| WorkflowError::Agent(e.to_string()))?,
                        None => Vec::new(),
                    };
                    let chain_override = definition.resolve_step_chain(step);
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
                            step.output_schema().clone(),
                        )
                        .await
                        .map_err(|e| WorkflowError::Agent(e.to_string()))?;
                    (agent, detached_agents)
                };
                op.commit()
                    .await
                    .map_err(|e| WorkflowError::Agent(e.to_string()))?;

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
                // doesn't leak into this agent's view of HEAD. Runs after
                // `notify_sandbox_attach` (which fired in
                // `create_for_workflow_run_in_op` above) so the freshly
                // refreshed token is available for `git fetch`. Best-effort.
                if let Some((sandbox_id, _)) = attach_sandbox {
                    self.sandboxes.reset_for_workflow_step(sandbox_id).await;
                }

                let result = self
                    .run_agent_until_submit_output(&agent, prompt, name, *timeout_seconds)
                    .await;

                // Detach the sandbox unconditionally so the next step can
                // attach (Write mode is single-writer). Best-effort; the
                // step-level result still propagates.
                if let Some((sandbox_id, _)) = attach_sandbox {
                    self.detach_step_sandbox(sandbox_id, agent.id).await;
                }

                result
            }
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
            .await
            .map_err(|e| WorkflowError::Agent(e.to_string()))?
        {
            Some(rx) => rx,
            None => match user_prompt {
                Some(prompt) => self
                    .agents
                    .send_message_with_choice(agent_subject, agent.id, prompt, tool_choice)
                    .await
                    .map_err(|e| WorkflowError::Agent(e.to_string()))?,
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
        self.agents
            .submitted_output(agent.id)
            .await
            .map_err(|e| WorkflowError::Agent(e.to_string()))
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
