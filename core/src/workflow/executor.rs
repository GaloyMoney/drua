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

    let mut any_failed = step_results_have_failure(&run);

    for step in &steps {
        let step_name = step.name().to_string();

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
            // `User(nil)` is treated as the system principal by
            // `AuthSubject::can` — bypasses authz the same way the
            // SA-token resolver does.
            let system_subject = AuthSubject::User(UserId::from(uuid::Uuid::nil()));

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

            let arguments = serde_json::to_string_pretty(trigger_context)
                .unwrap_or_else(|_| trigger_context.to_string());
            let sandbox_id = attach_sandbox.map(|(id, _)| id);
            let prompt = skills
                .interpolate_skill(skill, Some(workspace_id), sandbox_id, Some(&arguments))
                .await
                .map_err(|e| WorkflowError::Skill(e.to_string()))?
                .ok_or_else(|| WorkflowError::SkillNotFound(skill.clone()))?;

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

            let agent_subject = agent.auth_subject();
            let mut rx = agents
                .send_message(agent_subject, agent.id, prompt)
                .await
                .map_err(|e| WorkflowError::Agent(e.to_string()))?;

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
