use crate::session::{NewSessionEvent, SessionEventType};

use super::entity::{Agent, SandboxConfig, SandboxState};
use super::error::AgentError;
use super::repo::AgentRepo;
use super::AgentMessageEvent;

/// Translate a JSON-line from the agent harness (Claude Code stream-json)
/// into a canonical [`AgentMessageEvent`].
///
/// Only produces events meaningful for the SSE chat stream (text, tool_call,
/// tool_result, done, error).  System/init/stream_event lines produce nothing.
pub(super) fn translate_harness_event(line: &str) -> Vec<AgentMessageEvent> {
    let v = match serde_json::from_str::<serde_json::Value>(line) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let event_type = match v.get("type").and_then(|t| t.as_str()) {
        Some(t) => t,
        None => return Vec::new(),
    };

    match event_type {
        "assistant" => {
            let mut events = Vec::new();
            if let Some(blocks) = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            {
                for block in blocks {
                    match block.get("type").and_then(|t| t.as_str()) {
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                if !text.is_empty() {
                                    events.push(AgentMessageEvent::Text {
                                        text: text.to_string(),
                                    });
                                }
                            }
                        }
                        Some("tool_use") => {
                            let name = block
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("unknown")
                                .to_string();
                            let arguments = block.get("input").cloned();
                            events.push(AgentMessageEvent::ToolCall { name, arguments });
                        }
                        Some("tool_result") => {
                            let name = block
                                .get("name")
                                .or_else(|| block.get("tool_use_id"))
                                .and_then(|n| n.as_str())
                                .unwrap_or("unknown")
                                .to_string();
                            let is_error = block
                                .get("is_error")
                                .and_then(|e| e.as_bool())
                                .unwrap_or(false);
                            events.push(AgentMessageEvent::ToolResult { name, is_error });
                        }
                        _ => {}
                    }
                }
            }
            events
        }
        "result" => {
            let turns = v.get("num_turns").and_then(|n| n.as_u64()).unwrap_or(0) as u32;
            let usage = v.get("usage");
            let input_tokens = v
                .get("total_input_tokens")
                .or_else(|| usage.and_then(|u| u.get("input_tokens")))
                .and_then(|n| n.as_u64())
                .unwrap_or(0) as u32;
            let output_tokens = v
                .get("total_output_tokens")
                .or_else(|| usage.and_then(|u| u.get("output_tokens")))
                .and_then(|n| n.as_u64())
                .unwrap_or(0) as u32;
            let duration_ms = v.get("duration_ms").and_then(|n| n.as_u64());
            let cost_usd = v.get("total_cost_usd").and_then(|n| n.as_f64());
            vec![AgentMessageEvent::Done {
                turns,
                input_tokens,
                output_tokens,
                duration_ms,
                cost_usd,
            }]
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
            vec![AgentMessageEvent::Error { message }]
        }
        _ => Vec::new(), // system, init, stream_event — captured via session events only
    }
}

/// Translate a raw harness JSON line into rich [`NewSessionEvent`]s for
/// persistence.  Unlike [`translate_harness_event`] this captures **all**
/// event types including system/init/stream_event.
pub(super) fn translate_to_session_events(
    line: &str,
    raw: Option<serde_json::Value>,
) -> Vec<NewSessionEvent> {
    let v = match serde_json::from_str::<serde_json::Value>(line) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let event_type = match v.get("type").and_then(|t| t.as_str()) {
        Some(t) => t,
        None => return Vec::new(),
    };

    match event_type {
        "system" => {
            let subtype = v.get("subtype").and_then(|s| s.as_str()).unwrap_or("");
            match subtype {
                "init" => {
                    let session_id = v.get("session_id").and_then(|s| s.as_str());
                    let model = v.get("model").and_then(|s| s.as_str());
                    let tools = v.get("tools").cloned().unwrap_or(serde_json::json!([]));
                    vec![NewSessionEvent {
                        event_type: SessionEventType::SessionInit,
                        tool_name: None,
                        tool_use_id: None,
                        content: serde_json::json!({
                            "claude_session_id": session_id,
                            "model": model,
                            "tools": tools,
                        }),
                        metadata: serde_json::json!({}),
                        raw_event: raw,
                    }]
                }
                "api_retry" => {
                    vec![NewSessionEvent {
                        event_type: SessionEventType::ApiRetry,
                        tool_name: None,
                        tool_use_id: None,
                        content: serde_json::json!({
                            "attempt": v.get("attempt"),
                            "max_retries": v.get("max_retries"),
                            "retry_delay_ms": v.get("retry_delay_ms"),
                            "error": v.get("error"),
                        }),
                        metadata: serde_json::json!({}),
                        raw_event: raw,
                    }]
                }
                _ => Vec::new(),
            }
        }
        "assistant" => {
            let mut events = Vec::new();
            if let Some(blocks) = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            {
                for block in blocks {
                    match block.get("type").and_then(|t| t.as_str()) {
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                if !text.is_empty() {
                                    events.push(NewSessionEvent {
                                        event_type: SessionEventType::AssistantText,
                                        tool_name: None,
                                        tool_use_id: None,
                                        content: serde_json::json!({ "text": text }),
                                        metadata: serde_json::json!({}),
                                        raw_event: None,
                                    });
                                }
                            }
                        }
                        Some("tool_use") => {
                            let name = block
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("unknown");
                            let tool_use_id =
                                block.get("id").and_then(|id| id.as_str()).map(String::from);
                            let input =
                                block.get("input").cloned().unwrap_or(serde_json::json!({}));
                            events.push(NewSessionEvent {
                                event_type: SessionEventType::ToolCall,
                                tool_name: Some(name.to_string()),
                                tool_use_id,
                                content: serde_json::json!({ "input": input }),
                                metadata: serde_json::json!({}),
                                raw_event: None,
                            });
                        }
                        Some("tool_result") => {
                            let tool_use_id = block
                                .get("tool_use_id")
                                .and_then(|id| id.as_str())
                                .map(String::from);
                            let name = block.get("name").and_then(|n| n.as_str()).map(String::from);
                            let is_error = block
                                .get("is_error")
                                .and_then(|e| e.as_bool())
                                .unwrap_or(false);
                            let output = block
                                .get("content")
                                .cloned()
                                .unwrap_or(serde_json::json!(null));
                            events.push(NewSessionEvent {
                                event_type: SessionEventType::ToolResult,
                                tool_name: name,
                                tool_use_id,
                                content: serde_json::json!({
                                    "output": output,
                                    "is_error": is_error,
                                }),
                                metadata: serde_json::json!({}),
                                raw_event: None,
                            });
                        }
                        Some("thinking") => {
                            if let Some(text) = block.get("thinking").and_then(|t| t.as_str()) {
                                events.push(NewSessionEvent {
                                    event_type: SessionEventType::Thinking,
                                    tool_name: None,
                                    tool_use_id: None,
                                    content: serde_json::json!({ "text": text }),
                                    metadata: serde_json::json!({}),
                                    raw_event: None,
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            // Store raw on the first event of the batch (it covers the whole turn)
            if let Some(first) = events.first_mut() {
                first.raw_event = raw;
            }
            events
        }
        "result" => {
            let session_id = v.get("session_id").and_then(|s| s.as_str());
            let turns = v.get("num_turns").and_then(|n| n.as_u64()).unwrap_or(0);
            let input_tokens = v
                .get("total_input_tokens")
                .or_else(|| v.get("usage").and_then(|u| u.get("input_tokens")))
                .and_then(|n| n.as_u64())
                .unwrap_or(0);
            let output_tokens = v
                .get("total_output_tokens")
                .or_else(|| v.get("usage").and_then(|u| u.get("output_tokens")))
                .and_then(|n| n.as_u64())
                .unwrap_or(0);
            let duration_ms = v.get("duration_ms").and_then(|n| n.as_u64());
            let cost_usd = v.get("total_cost_usd").and_then(|n| n.as_f64());
            vec![NewSessionEvent {
                event_type: SessionEventType::SessionEnd,
                tool_name: None,
                tool_use_id: None,
                content: serde_json::json!({
                    "claude_session_id": session_id,
                    "turns": turns,
                    "input_tokens": input_tokens,
                    "output_tokens": output_tokens,
                    "duration_ms": duration_ms,
                    "cost_usd": cost_usd,
                }),
                metadata: serde_json::json!({}),
                raw_event: raw,
            }]
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
            vec![NewSessionEvent {
                event_type: SessionEventType::Error,
                tool_name: None,
                tool_use_id: None,
                content: serde_json::json!({ "message": message }),
                metadata: serde_json::json!({}),
                raw_event: raw,
            }]
        }
        // stream_event deltas are high-volume and reconstruct into the
        // complete `assistant` event — skip for storage.
        _ => Vec::new(),
    }
}

/// Clone the base sandbox client and apply agent-specific configuration.
///
/// All sandboxes get a persistent volume for session history.
pub(super) fn configure_client(
    base: &sandbox_client::SandboxClient,
    config: &SandboxConfig,
    storage_class: &str,
) -> sandbox_client::SandboxClient {
    let mut client = base.clone();
    client = client.with_persistence(sandbox_client::PersistenceConfig {
        size: config.pvc_size.clone(),
        storage_class: storage_class.to_string(),
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
                    // Check if the sandbox image is up-to-date with the template
                    match client.is_sandbox_current(&sandbox_name).await {
                        Ok(true) => return Ok(sandbox_name),
                        Ok(false) => {
                            emit_status(tx, "Updating sandbox image…").await;
                            let _ = client.delete_sandbox(&sandbox_name).await;
                            if agent.sandbox_lost().did_execute() {
                                repo.update(&mut agent).await?;
                            }
                            // Fall through to creation below
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "Failed to check sandbox image, continuing");
                            return Ok(sandbox_name);
                        }
                    }
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

    let extra_env = vec![("AGENT_ID".to_string(), agent.id.to_string())];
    match client.create_sandbox(&sandbox_name, extra_env).await {
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
