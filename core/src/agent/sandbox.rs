use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::entity::{Agent, SandboxConfig, SandboxState};
use super::error::AgentError;
use super::repo::AgentRepo;
use super::AgentMessageEvent;

/// Execute the agent harness inside a sandbox pod and relay stdout → channel.
pub(super) async fn relay_agent_message(
    client: Arc<sandbox_client::SandboxClient>,
    sandbox_name: &str,
    prompt: String,
    session_id: Option<String>,
    model: Option<String>,
    max_turns: Option<u32>,
    tx: tokio::sync::mpsc::Sender<AgentMessageEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let command = vec!["agent-harness".to_string()];
    let mut process = client.exec_sandbox_raw(sandbox_name, command).await?;

    let input_line = serde_json::json!({
        "prompt": prompt,
        "session_id": session_id,
        "model": model,
        "max_turns": max_turns,
    });

    let mut stdin = process
        .stdin()
        .ok_or("no stdin stream from agent harness")?;
    let payload = format!("{}\n", input_line);
    stdin.write_all(payload.as_bytes()).await?;
    drop(stdin);

    let mut stdout = process
        .stdout()
        .ok_or("no stdout stream from agent harness")?;

    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];

    loop {
        match stdout.read(&mut tmp).await {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);

                while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=pos).collect();
                    let line_str = String::from_utf8_lossy(&line).trim().to_string();
                    if line_str.is_empty() {
                        continue;
                    }

                    if let Some(event) = translate_harness_event(&line_str) {
                        if tx.send(event).await.is_err() {
                            return Ok(());
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Agent harness stdout error");
                break;
            }
        }
    }

    // Flush remaining partial line
    if !buf.is_empty() {
        let line_str = String::from_utf8_lossy(&buf).trim().to_string();
        if !line_str.is_empty() {
            if let Some(event) = translate_harness_event(&line_str) {
                let _ = tx.send(event).await;
            }
        }
    }

    Ok(())
}

/// Translate a JSON-line from the agent harness (Claude Agent SDK) into a
/// canonical [`AgentMessageEvent`].
fn translate_harness_event(line: &str) -> Option<AgentMessageEvent> {
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
            let message = v
                .get("message")
                .or_else(|| v.get("details"))
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error")
                .to_string();
            Some(AgentMessageEvent::Error { message })
        }
        _ => None, // Ignore system, init, and other SDK-internal events
    }
}

/// Clone the base sandbox client and apply agent-specific configuration.
pub(super) fn configure_client(
    base: &sandbox_client::SandboxClient,
    config: &SandboxConfig,
) -> sandbox_client::SandboxClient {
    let client = base.clone();
    if config.persistent_volume {
        client.with_persistence(sandbox_client::PersistenceConfig {
            size: config.pvc_size.clone(),
            storage_class: String::new(),
            mount_path: "/workspace".to_string(),
        })
    } else {
        client
    }
}

/// Ensure the agent has a running sandbox, creating one if needed.
/// If the entity thinks a sandbox exists but it was deleted externally
/// (e.g. namespace recreation), reset state and recreate.
pub(super) async fn ensure_sandbox(
    client: &sandbox_client::SandboxClient,
    agent: &mut Agent,
    repo: &AgentRepo,
) -> Result<String, AgentError> {
    let sandbox_name = agent.sandbox_name();

    match agent.sandbox_state {
        SandboxState::Ready | SandboxState::Provisioning => {
            match client.get_sandbox(&sandbox_name).await {
                Ok(_) if agent.sandbox_state == SandboxState::Ready => {
                    return Ok(sandbox_name);
                }
                Ok(_) => {
                    // Provisioning — wait for ready
                    client
                        .wait_sandbox_ready(&sandbox_name, std::time::Duration::from_secs(120))
                        .await?;
                    if agent.sandbox_ready().did_execute() {
                        repo.update(agent).await?;
                    }
                    return Ok(sandbox_name);
                }
                Err(sandbox_client::SandboxError::NotFound(_)) => {
                    tracing::warn!(
                        sandbox = %sandbox_name,
                        "Sandbox missing from cluster, resetting state"
                    );
                    if agent.sandbox_lost().did_execute() {
                        repo.update(agent).await?;
                    }
                    // Fall through to creation below
                }
                Err(e) => return Err(e.into()),
            }
        }
        SandboxState::None => {}
    }

    // Try to create; if already exists, just wait for ready
    match client.create_sandbox(&sandbox_name).await {
        Ok(_) => {}
        Err(sandbox_client::SandboxError::Kube(e)) if e.to_string().contains("already exists") => {
            tracing::info!(sandbox = %sandbox_name, "Sandbox already exists, waiting for ready");
        }
        Err(e) => return Err(e.into()),
    }

    if agent.sandbox_provisioned().did_execute() {
        repo.update(agent).await?;
    }

    client
        .wait_sandbox_ready(&sandbox_name, std::time::Duration::from_secs(120))
        .await?;

    if agent.sandbox_ready().did_execute() {
        repo.update(agent).await?;
    }

    Ok(sandbox_name)
}
