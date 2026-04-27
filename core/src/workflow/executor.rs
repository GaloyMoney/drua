//! Workflow run executor.
//!
//! For the MVP this is intentionally minimal: it walks the snapshotted
//! `steps_snapshot` of a [`WorkflowRun`] in order and executes each
//! `AgentStep` by spinning up a transient agent, sending the resolved
//! skill body as the prompt (with `$ARGUMENTS` substituted to the
//! pretty-printed trigger context), draining the chat-output stream into
//! a single string, and recording the outcome on the run.

use std::sync::Arc;
use std::time::Duration;

use crate::agent::Agents;
use crate::auth::AuthSubject;
use crate::primitives::*;
use crate::sandbox::{SandboxAgentMode, Sandboxes};
use crate::skill::Skills;

use crate::primitives::ChatOutputEvent;

use super::definition::WorkflowStepDef;
use super::error::WorkflowError;
use super::run::{WorkflowRun, WorkflowRunRepo, WorkflowRunState};

/// Run an existing [`WorkflowRun`] to completion.
///
/// Loads the run, walks its snapshotted steps, and persists step
/// transitions + the terminal `RunCompleted` event. Errors during a step
/// are caught and turned into `StepFailed` + a failed terminal state —
/// the function returns `Ok(())` whenever it managed to record a
/// terminal state, and only `Err(_)` when the run cannot even be loaded
/// or persisted.
///
/// **Resumable.** All entity mutations are idempotent (see
/// [`WorkflowRun::step_started`] etc.) and steps that already finished
/// in a prior attempt are skipped, so the function is safe to invoke
/// repeatedly by the job runner after a crash or restart.
#[tracing::instrument(name = "core.workflow.execute_run", skip_all, fields(run_id = %run_id))]
pub async fn execute_run(
    runs: WorkflowRunRepo,
    agents: Arc<Agents>,
    skills: Arc<Skills>,
    sandboxes: Arc<Sandboxes>,
    run_id: WorkflowRunId,
) -> Result<(), WorkflowError> {
    let mut run = runs.find_by_id(run_id).await?;

    // If a prior attempt already wrote the terminal `RunCompleted`
    // event there's nothing left to do — the job runner can mark this
    // attempt as complete and move on.
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

    let mut any_failed = step_results_have_failure(&run);

    for step in &steps {
        let step_name = step.name().to_string();

        // Skip steps that already terminated in a prior execution attempt.
        if step_already_terminal(&run, &step_name) {
            continue;
        }

        if run.step_started(step_name.clone()).did_execute() {
            runs.update(&mut run).await?;
        }

        let outcome = execute_step(
            &agents,
            &skills,
            &sandboxes,
            workspace_id,
            workflow_id,
            run_id,
            step,
            &trigger_context,
        )
        .await;

        match outcome {
            Ok(output) => {
                if run.step_completed(step_name, output).did_execute() {
                    runs.update(&mut run).await?;
                }
            }
            Err(err) => {
                any_failed = true;
                if run.step_failed(step_name, err.to_string()).did_execute() {
                    runs.update(&mut run).await?;
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
        runs.update(&mut run).await?;
    }
    Ok(())
}

fn step_already_terminal(run: &WorkflowRun, step_name: &str) -> bool {
    run.step_results
        .iter()
        .any(|r| r.name == step_name && r.completed_at.is_some())
}

fn step_results_have_failure(run: &WorkflowRun) -> bool {
    run.step_results.iter().any(|r| r.error.is_some())
}

#[allow(clippy::too_many_arguments)]
async fn execute_step(
    agents: &Agents,
    skills: &Skills,
    sandboxes: &Sandboxes,
    workspace_id: WorkspaceId,
    workflow_id: WorkflowDefinitionId,
    run_id: WorkflowRunId,
    step: &WorkflowStepDef,
    trigger_context: &serde_json::Value,
) -> Result<serde_json::Value, WorkflowError> {
    match step {
        WorkflowStepDef::AgentStep {
            name,
            skill,
            sandbox,
            timeout_seconds,
        } => {
            // System-level user subject — nil UUID, treated as a real
            // user and therefore allowed everything by `AuthSubject::can`.
            let system_subject = AuthSubject::User(UserId::from(uuid::Uuid::nil()));

            // Resolve the sandbox by name (within the workspace) if requested.
            let attach_sandbox = match sandbox.as_deref() {
                Some(sandbox_name) => {
                    let list = sandboxes
                        .list_for_workspace(&system_subject, workspace_id)
                        .await
                        .map_err(|e| WorkflowError::Sandbox(e.to_string()))?;
                    let found = list
                        .into_iter()
                        .find(|s| s.name == sandbox_name)
                        .ok_or_else(|| WorkflowError::SandboxNotFound(sandbox_name.to_string()))?;
                    Some((found.id, SandboxAgentMode::Write))
                }
                None => None,
            };

            // Build the prompt by interpolating `$ARGUMENTS` in the skill
            // body with the (pretty-printed) trigger context.
            let arguments = serde_json::to_string_pretty(trigger_context)
                .unwrap_or_else(|_| trigger_context.to_string());
            let sandbox_id = attach_sandbox.map(|(id, _)| id);
            let prompt = skills
                .interpolate_skill(skill, Some(workspace_id), sandbox_id, Some(&arguments))
                .await
                .map_err(|e| WorkflowError::Skill(e.to_string()))?
                .ok_or_else(|| WorkflowError::SkillNotFound(skill.clone()))?;

            // Spawn an agent owned by this workflow run. The
            // `(workflow_id, workflow_run_id)` stamp keeps it out of
            // the default workspace agent listing — see
            // `Agents::list_for_workspace`.
            let agent_name = format!("workflow-{}-{}", run_id, name);
            let mut op = agents
                .begin_op()
                .await
                .map_err(|e| WorkflowError::Agent(e.to_string()))?;
            let agent = agents
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

            // Send the prompt as the agent's auth subject so its tool
            // calls authorize correctly.
            let agent_subject = agent.auth_subject();
            let mut rx = agents
                .send_message(agent_subject, agent.id, prompt)
                .await
                .map_err(|e| WorkflowError::Agent(e.to_string()))?;

            // Drain the chat-output stream, collecting AssistantText +
            // AssistantTextDelta into a single string. Terminate on
            // AssistantDone, Error, or channel close.
            let timeout = timeout_seconds
                .map(Duration::from_secs)
                .unwrap_or_else(|| Duration::from_secs(300));

            let collect = async {
                let mut output = String::new();
                let mut error: Option<String> = None;
                while let Some(event) = rx.recv().await {
                    match event {
                        ChatOutputEvent::AssistantText { text } => {
                            output.push_str(&text);
                        }
                        ChatOutputEvent::AssistantTextDelta { text } => {
                            output.push_str(&text);
                        }
                        ChatOutputEvent::AssistantDone { .. } => break,
                        ChatOutputEvent::Error { message } => {
                            error = Some(message);
                            break;
                        }
                        _ => {}
                    }
                }
                (output, error)
            };

            let (output, err) = tokio::time::timeout(timeout, collect).await.map_err(|_| {
                WorkflowError::StepFailed {
                    step: name.clone(),
                    reason: format!("timeout after {}s", timeout.as_secs()),
                }
            })?;

            if let Some(message) = err {
                return Err(WorkflowError::StepFailed {
                    step: name.clone(),
                    reason: message,
                });
            }

            Ok(serde_json::Value::String(output))
        }
    }
}
