use std::sync::Arc;
use std::time::Duration;

use crate::agent::Agents;
use crate::primitives::{ChatOutputEvent, WorkflowDefinitionId, WorkflowRunId, WorkspaceId};
use crate::sandbox::{SandboxAgentMode, Sandboxes};
use crate::skill::Skills;

use super::definition::WorkflowStepDef;
use super::error::WorkflowError;
use super::run::{WorkflowRunRepo, WorkflowRunState};

/// Resumable: idempotent mutations + skipping already-terminal steps
/// make this safe to invoke repeatedly after a crash.
/// `Ok(())` whenever a terminal state was recorded; `Err(_)` only on
/// load/persist failure.
#[tracing::instrument(name = "core.workflow.execute_run", skip_all, fields(run_id = %run_id))]
pub async fn execute_run(
    runs: WorkflowRunRepo,
    agents: Arc<Agents>,
    skills: Arc<Skills>,
    sandboxes: Arc<Sandboxes>,
    run_id: WorkflowRunId,
) -> Result<(), WorkflowError> {
    let mut run = runs.find_by_id(run_id).await?;

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

    let mut any_failed = run.any_step_failed();

    for step in &steps {
        let step_name = step.name().to_string();

        if run.step_already_terminal(&step_name) {
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
            sandbox_mode,
            timeout_seconds,
        } => {
            let attach_sandbox = match sandbox.as_deref() {
                Some(sandbox_name) => {
                    let found = sandboxes
                        .find_for_workflow_run(workspace_id, sandbox_name)
                        .await
                        .map_err(|e| WorkflowError::Sandbox(e.to_string()))?
                        .ok_or_else(|| WorkflowError::SandboxNotFound(sandbox_name.to_string()))?;
                    Some((found.id, sandbox_mode.unwrap_or(SandboxAgentMode::Write)))
                }
                None => None,
            };

            let arguments = serde_json::to_string_pretty(trigger_context)
                .unwrap_or_else(|_| trigger_context.to_string());
            let sandbox_id = attach_sandbox.map(|(id, _)| id);
            let prompt = skills
                .interpolate_skill(skill, Some(workspace_id), sandbox_id, Some(&arguments))
                .await
                .map_err(|e| WorkflowError::Skill(e.to_string()))?
                .ok_or_else(|| WorkflowError::SkillNotFound(skill.clone()))?;

            let run_id_short = {
                let s = run_id.to_string();
                s.split_once('-').map(|(p, _)| p.to_string()).unwrap_or(s)
            };
            let agent_name = format!("workflow-{run_id_short}-{name}");
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

            let agent_subject = agent.auth_subject();
            let mut rx = agents
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
                            step: name.clone(),
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
                            step: name.clone(),
                            reason: message,
                        });
                    }
                    _ => {}
                }
            }

            Ok(serde_json::Value::String(output))
        }
    }
}
