use super::entity::{Agent, SandboxConfig, SandboxState};
use super::error::AgentError;
use super::repo::AgentRepo;
use super::AgentMessageEvent;

/// Translate a JSON-line from the agent harness (Claude Agent SDK) into a
/// canonical [`AgentMessageEvent`].
pub(super) fn translate_harness_event(line: &str) -> Option<AgentMessageEvent> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let event_type = v.get("type")?.as_str()?;

    match event_type {
        "assistant" => {
            let text = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join("")
                })
                .unwrap_or_default();
            if text.is_empty() {
                None
            } else {
                Some(AgentMessageEvent::Text { text })
            }
        }
        "result" => {
            let turns = v.get("num_turns").and_then(|n| n.as_u64()).unwrap_or(0) as u32;
            let input_tokens = v
                .get("total_input_tokens")
                .or_else(|| v.get("input_tokens"))
                .and_then(|n| n.as_u64())
                .unwrap_or(0) as u32;
            let output_tokens = v
                .get("total_output_tokens")
                .or_else(|| v.get("output_tokens"))
                .and_then(|n| n.as_u64())
                .unwrap_or(0) as u32;
            Some(AgentMessageEvent::Done {
                turns,
                input_tokens,
                output_tokens,
            })
        }
        "error" => {
            let msg = v
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            let details = v.get("details").and_then(|d| d.as_str());
            let message = match details {
                Some(d) => format!("{msg}: {d}"),
                None => msg.to_string(),
            };
            Some(AgentMessageEvent::Error { message })
        }
        _ => None, // Ignore system, init, and other SDK-internal events
    }
}

/// Clone the base sandbox client and apply agent-specific configuration.
///
/// All sandboxes get a persistent volume for session history.
pub(super) fn configure_client(
    base: &sandbox_client::SandboxClient,
    config: &SandboxConfig,
) -> sandbox_client::SandboxClient {
    let mut client = base.clone();
    client = client.with_persistence(sandbox_client::PersistenceConfig {
        size: config.pvc_size.clone(),
        storage_class: String::new(),
        mount_path: "/workspace".to_string(),
    });
    if !config.resource_cpu.is_empty() || !config.resource_mem.is_empty() {
        client = client.with_resources(sandbox_client::ResourceConfig {
            cpu: config.resource_cpu.clone(),
            memory: config.resource_mem.clone(),
        });
    }
    client
}

/// Send a service status message to the UI (best-effort, non-blocking).
async fn emit_status(
    tx: &tokio::sync::mpsc::Sender<AgentMessageEvent>,
    message: impl Into<String>,
) {
    let _ = tx
        .send(AgentMessageEvent::Service {
            message: message.into(),
        })
        .await;
}

/// Ensure the agent has a running sandbox, creating one if needed.
/// Emits [`AgentMessageEvent::Service`] messages through `tx` so the UI
/// can show provisioning progress.
pub(super) async fn ensure_sandbox(
    client: &sandbox_client::SandboxClient,
    mut agent: Agent,
    repo: &AgentRepo,
    tx: &tokio::sync::mpsc::Sender<AgentMessageEvent>,
) -> Result<String, AgentError> {
    let sandbox_name = agent.sandbox_name();

    match agent.sandbox_state {
        SandboxState::Ready | SandboxState::Provisioning => {
            match client.get_sandbox(&sandbox_name).await {
                Ok(_) if agent.sandbox_state == SandboxState::Ready => {
                    return Ok(sandbox_name);
                }
                Ok(_) => {
                    emit_status(tx, "Waiting for sandbox to be ready…").await;
                    let timeout = std::time::Duration::from_secs(120);
                    client.wait_sandbox_ready(&sandbox_name, timeout).await?;
                    if agent.sandbox_ready().did_execute() {
                        repo.update(&mut agent).await?;
                    }
                    return Ok(sandbox_name);
                }
                Err(sandbox_client::SandboxError::NotFound(_)) => {
                    tracing::warn!(
                        sandbox = %sandbox_name,
                        "Sandbox missing from cluster, resetting state"
                    );
                    emit_status(tx, "Recreating sandbox…").await;
                    if agent.sandbox_lost().did_execute() {
                        repo.update(&mut agent).await?;
                    }
                    // Fall through to creation below
                }
                Err(e) => return Err(e.into()),
            }
        }
        SandboxState::None => {
            emit_status(tx, "Creating sandbox…").await;
        }
    }

    match client.create_sandbox(&sandbox_name).await {
        Ok(_) => {}
        Err(sandbox_client::SandboxError::Kube(e)) if e.to_string().contains("already exists") => {
            tracing::info!(sandbox = %sandbox_name, "Sandbox already exists, waiting for ready");
        }
        Err(e) => return Err(e.into()),
    }

    if agent.sandbox_provisioned().did_execute() {
        repo.update(&mut agent).await?;
    }

    emit_status(tx, "Waiting for sandbox to be ready…").await;
    client
        .wait_sandbox_ready(&sandbox_name, std::time::Duration::from_secs(120))
        .await?;

    if agent.sandbox_ready().did_execute() {
        repo.update(&mut agent).await?;
    }

    Ok(sandbox_name)
}
