use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::agent::{Agent, Agents};
use crate::primitives::{
    AgentId, ChatOutputEvent, SandboxId, WorkflowDefinitionId, WorkflowRunId, WorkspaceId,
};
use crate::sandbox::{SandboxAgentMode, SandboxState, Sandboxes};
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
    /// load/persist failure.
    #[tracing::instrument(name = "core.workflow.execute_run", skip_all, fields(run_id = %run_id))]
    pub async fn run(&self, run_id: WorkflowRunId) -> Result<(), WorkflowError> {
        let mut run = self.runs.find_by_id(run_id).await?;

        if matches!(
            run.state,
            WorkflowRunState::Succeeded | WorkflowRunState::Failed
        ) {
            return Ok(());
        }

        let workspace_id = run.workspace_id;
        let workflow_id = run.definition_id;
        let trigger_context = run.trigger_context.clone();
        let steps = run.steps_snapshot.clone();

        // Stamp every audit row recorded during this run so they can be
        // queried by `resource_ids->>'workflow_run_id'`.
        crate::audit::Audit::record_workspace_id(workspace_id);
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
        let sandbox_ids = match self
            .ensure_sandboxes_ready(workspace_id, workflow_id, &sandbox_decls)
            .await
        {
            Ok(map) => map,
            Err(err) => {
                let step_name = "<pre-flight>".to_string();
                if run.step_started(step_name.clone()).did_execute() {
                    self.runs.update(&mut run).await?;
                }
                if run.step_failed(step_name, err.to_string()).did_execute() {
                    self.runs.update(&mut run).await?;
                }
                if run.run_completed(WorkflowRunState::Failed).did_execute() {
                    self.runs.update(&mut run).await?;
                }
                self.suspend_workflow_sandboxes(workspace_id, workflow_id)
                    .await;
                return Ok(());
            }
        };

        let mut any_failed = run.any_step_failed();

        for step in &steps {
            let step_name = step.name().to_string();

            if run.step_already_terminal(&step_name) {
                continue;
            }

            if run.step_started(step_name.clone()).did_execute() {
                self.runs.update(&mut run).await?;
            }

            let outcome = self
                .execute_step(
                    workspace_id,
                    workflow_id,
                    run_id,
                    step,
                    &trigger_context,
                    &sandbox_ids,
                )
                .await;

            match outcome {
                Ok(output) => {
                    if run.step_completed(step_name, output).did_execute() {
                        self.runs.update(&mut run).await?;
                    }
                }
                Err(err) => {
                    any_failed = true;
                    if run.step_failed(step_name, err.to_string()).did_execute() {
                        self.runs.update(&mut run).await?;
                    }
                    break;
                }
            }
        }

        let terminal = if any_failed {
            WorkflowRunState::Failed
        } else {
            WorkflowRunState::Succeeded
        };
        if run.run_completed(terminal).did_execute() {
            self.runs.update(&mut run).await?;
        }

        // Post-flight: suspend every workflow-scoped sandbox. Always
        // runs (even when a step failed). Best-effort.
        self.suspend_workflow_sandboxes(workspace_id, workflow_id)
            .await;

        Ok(())
    }

    /// For each declared sandbox: find or create scoped to the workflow,
    /// restart if Suspended, then wait until Ready. Returns a name → id
    /// map the step loop uses to resolve `sandbox: Some(name)` references.
    async fn ensure_sandboxes_ready(
        &self,
        workspace_id: WorkspaceId,
        workflow_id: WorkflowDefinitionId,
        decls: &[WorkflowSandboxDecl],
    ) -> Result<HashMap<String, SandboxId>, WorkflowError> {
        let mut ids: HashMap<String, SandboxId> = HashMap::with_capacity(decls.len());
        for decl in decls {
            let (decl_name, sandbox) = match decl {
                // Reference an already-existing sandbox in the
                // workspace by its (unique) name; attach only.
                WorkflowSandboxDecl::Preexisting { name } => {
                    let sb = self
                        .sandboxes
                        .find_by_name_in_workspace_unchecked(workspace_id, name)
                        .await
                        .map_err(|e| {
                            WorkflowError::SandboxNotFound(format!(
                                "preexisting sandbox '{name}': {e}"
                            ))
                        })?;
                    (name, sb)
                }
                // Workflow-scoped sandbox: find / create / restart.
                WorkflowSandboxDecl::Provisioned {
                    name,
                    mode,
                    specs: _,
                } => {
                    let existing = self
                        .sandboxes
                        .find_for_workflow(workspace_id, workflow_id, name)
                        .await
                        .map_err(|e| WorkflowError::Sandbox(e.to_string()))?;

                    let sandbox = match existing {
                        None => {
                            let specs = decl
                                .specs_or_default()
                                .expect("specs_or_default returns Some for Provisioned decls");
                            let mut op = self
                                .sandboxes
                                .begin_op()
                                .await
                                .map_err(|e| WorkflowError::Sandbox(e.to_string()))?;
                            let sb = self
                                .sandboxes
                                .create_for_workflow_in_op(
                                    &mut op,
                                    workspace_id,
                                    workflow_id,
                                    name.clone(),
                                    specs,
                                    mode.clone(),
                                )
                                .await
                                .map_err(|e| WorkflowError::Sandbox(e.to_string()))?;
                            op.commit()
                                .await
                                .map_err(|e| WorkflowError::Sandbox(e.to_string()))?;
                            self.sandboxes.spawn_sandbox_creation(sb.id);
                            sb
                        }
                        Some(sb)
                            if matches!(
                                sb.state,
                                SandboxState::Suspended | SandboxState::Errored
                            ) =>
                        {
                            if sb.state == SandboxState::Errored {
                                tracing::warn!(
                                    sandbox_id = %sb.id,
                                    sandbox_name = %name,
                                    last_error = ?sb.last_error,
                                    "pre-flight: restarting sandbox in Errored state",
                                );
                            }
                            self.sandboxes
                                .restart_for_workflow(sb.id)
                                .await
                                .map_err(|e| WorkflowError::Sandbox(e.to_string()))?
                        }
                        Some(sb) => sb,
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
            ids.insert(decl_name.clone(), sandbox.id);
        }
        Ok(ids)
    }

    async fn suspend_workflow_sandboxes(
        &self,
        workspace_id: WorkspaceId,
        workflow_id: WorkflowDefinitionId,
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
            // Preexisting sandboxes are user-managed; never suspend them.
            let WorkflowSandboxDecl::Provisioned { name, .. } = decl else {
                continue;
            };
            let sandbox = match self
                .sandboxes
                .find_for_workflow(workspace_id, workflow_id, name)
                .await
            {
                Ok(Some(sb)) => sb,
                Ok(None) => continue,
                Err(e) => {
                    tracing::warn!(error = %e, sandbox = %name, "post-flight: lookup failed");
                    continue;
                }
            };
            if sandbox.state == SandboxState::Suspended {
                continue;
            }
            if let Err(e) = self.sandboxes.suspend_in_op(&mut op, sandbox.id).await {
                tracing::warn!(error = %e, sandbox_id = %sandbox.id, "post-flight: suspend failed");
            }
        }
        if let Err(e) = op.commit().await {
            tracing::warn!(error = %e, "post-flight: suspend commit failed");
        }
    }

    async fn execute_step(
        &self,
        workspace_id: WorkspaceId,
        workflow_id: WorkflowDefinitionId,
        run_id: WorkflowRunId,
        step: &WorkflowStepDef,
        trigger_context: &serde_json::Value,
        sandbox_ids: &HashMap<String, SandboxId>,
    ) -> Result<serde_json::Value, WorkflowError> {
        match step {
            WorkflowStepDef::AgentStep {
                name,
                skill,
                sandbox,
                sandbox_mode,
                timeout_seconds,
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
                    .interpolate_skill(skill, Some(workspace_id), sandbox_id, Some(&arguments))
                    .await
                    .map_err(|e| WorkflowError::Skill(e.to_string()))?
                    .ok_or_else(|| WorkflowError::SkillNotFound(skill.clone()))?;

                let run_id_short = {
                    let s = run_id.to_string();
                    s.split_once('-').map(|(p, _)| p.to_string()).unwrap_or(s)
                };
                let agent_name = format!("workflow-{run_id_short}-{name}");
                let mut op = self
                    .agents
                    .begin_op()
                    .await
                    .map_err(|e| WorkflowError::Agent(e.to_string()))?;
                let agent = self
                    .agents
                    .create_for_workflow_run_in_op(
                        &mut op,
                        workspace_id,
                        workflow_id,
                        run_id,
                        &agent_name,
                        attach_sandbox,
                    )
                    .await
                    .map_err(|e| WorkflowError::Agent(e.to_string()))?;
                op.commit()
                    .await
                    .map_err(|e| WorkflowError::Agent(e.to_string()))?;

                let result = self
                    .stream_agent_response(&agent, prompt, name, *timeout_seconds)
                    .await;

                // Detach the sandbox unconditionally so the next step can
                // attach (Write mode is single-writer). Best-effort; the
                // step-level result still propagates.
                if let Some((sandbox_id, _)) = attach_sandbox {
                    self.detach_step_sandbox(sandbox_id, agent.id).await;
                }

                result.map(serde_json::Value::String)
            }
        }
    }

    async fn stream_agent_response(
        &self,
        agent: &Agent,
        prompt: String,
        step_name: &str,
        timeout_seconds: Option<u64>,
    ) -> Result<String, WorkflowError> {
        let agent_subject = agent.auth_subject();
        let mut rx = self
            .agents
            .send_message(agent_subject, agent.id, prompt)
            .await
            .map_err(|e| WorkflowError::Agent(e.to_string()))?;

        // Idle timeout: resets on every streamed event so a busy agent
        // can run indefinitely. Trips only when nothing arrives for the
        // configured window.
        let idle_timeout = timeout_seconds
            .map(Duration::from_secs)
            .unwrap_or_else(|| Duration::from_secs(300));

        let mut output = String::new();
        loop {
            let event = match tokio::time::timeout(idle_timeout, rx.recv()).await {
                Ok(Some(event)) => event,
                Ok(None) => break,
                Err(_) => {
                    return Err(WorkflowError::StepFailed {
                        step: step_name.to_string(),
                        reason: format!("idle for {}s", idle_timeout.as_secs()),
                    });
                }
            };
            match event {
                ChatOutputEvent::AssistantText { text } => output.push_str(&text),
                ChatOutputEvent::AssistantTextDelta { text } => output.push_str(&text),
                ChatOutputEvent::AssistantDone { .. } => break,
                ChatOutputEvent::Error { message } => {
                    return Err(WorkflowError::StepFailed {
                        step: step_name.to_string(),
                        reason: message,
                    });
                }
                _ => {}
            }
        }
        Ok(output)
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
