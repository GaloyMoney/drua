use std::io::Write;

use anyhow::Result;
use serde::Deserialize;
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::graphql::GraphqlClient;

// ---------------------------------------------------------------------------
// Agent resolution
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct AgentVerifyResponse {
    agent: Option<AgentIdOnly>,
}

#[derive(Debug, Deserialize)]
struct AgentIdOnly {
    id: String,
}

async fn verify_agent(client: &GraphqlClient, agent_id: &str) -> Result<bool> {
    let query = "query($id: AgentId!) { agent(id: $id) { id } }";
    let resp: AgentVerifyResponse = client
        .query(query, serde_json::json!({ "id": agent_id }))
        .await?;
    Ok(resp.agent.is_some())
}

#[derive(Debug, Deserialize)]
struct WorkspaceCreateResponse {
    #[serde(rename = "workspaceCreate")]
    workspace_create: WorkspaceCreatePayload,
}

#[derive(Debug, Deserialize)]
struct WorkspaceCreatePayload {
    workspace: CreatedWorkspace,
}

#[derive(Debug, Deserialize)]
struct CreatedWorkspace {
    lead: Option<AgentIdOnly>,
}

async fn provision_workspace(client: &GraphqlClient) -> Result<String> {
    let query = r#"
        mutation WorkspaceCreate($input: WorkspaceCreateInput!) {
            workspaceCreate(input: $input) {
                workspace {
                    lead { id }
                }
            }
        }
    "#;

    let input = serde_json::json!({
        "name": "drua-chat",
        "description": "Auto-provisioned workspace for drua chat"
    });

    let resp: WorkspaceCreateResponse = client
        .query(query, serde_json::json!({ "input": input }))
        .await?;

    resp.workspace_create
        .workspace
        .lead
        .map(|a| a.id)
        .ok_or_else(|| anyhow::anyhow!("workspace created but no lead agent returned"))
}

async fn ensure_agent(
    config: &mut Config,
    client: &GraphqlClient,
    explicit_agent: Option<String>,
) -> Result<String> {
    if let Some(id) = explicit_agent {
        return Ok(id);
    }

    if let Some(ref id) = config.chat_agent_id {
        match verify_agent(client, id).await {
            Ok(true) => return Ok(id.clone()),
            Ok(false) => {
                eprintln!("Stored agent no longer exists, creating new workspace...");
            }
            Err(e) => {
                eprintln!("Could not verify stored agent: {e}");
                eprintln!("Creating new workspace...");
            }
        }
    }

    eprintln!("Provisioning chat workspace...");
    let agent_id = provision_workspace(client).await?;
    config.chat_agent_id = Some(agent_id.clone());
    config.save()?;
    Ok(agent_id)
}

// ---------------------------------------------------------------------------
// Stream events
// ---------------------------------------------------------------------------

enum StreamEvent {
    Delta(String),
    ToolStart(String),
    ToolResult { name: String, is_error: bool },
    Service(String),
    Error(String),
    Done(UsageInfo),
}

struct UsageInfo {
    turns: u32,
    input_tokens: u32,
    output_tokens: u32,
    duration_ms: Option<u32>,
    cost_usd: Option<f64>,
}

fn parse_stream_event(event: &serde_json::Value) -> Option<StreamEvent> {
    let typename = event.get("__typename")?.as_str()?;
    match typename {
        "TextDeltaEvent" => {
            let text = event.get("text")?.as_str()?;
            Some(StreamEvent::Delta(text.to_string()))
        }
        "ToolCallStartEvent" => {
            let name = event
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("tool");
            Some(StreamEvent::ToolStart(name.to_string()))
        }
        "ToolResultEvent" => {
            let name = event
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("tool")
                .to_string();
            let is_error = event
                .get("isError")
                .and_then(|e| e.as_bool())
                .unwrap_or(false);
            Some(StreamEvent::ToolResult { name, is_error })
        }
        "ServiceEvent" => {
            let msg = event.get("message")?.as_str()?;
            Some(StreamEvent::Service(msg.to_string()))
        }
        "ErrorEvent" => {
            let msg = event
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            Some(StreamEvent::Error(msg.to_string()))
        }
        "AssistantDoneEvent" => {
            let turns = event
                .get("turns")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            let input_tokens = event
                .get("inputTokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            let output_tokens = event
                .get("outputTokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            let duration_ms = event
                .get("durationMs")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            let cost_usd = event.get("costUsd").and_then(|v| v.as_f64());
            Some(StreamEvent::Done(UsageInfo {
                turns,
                input_tokens,
                output_tokens,
                duration_ms,
                cost_usd,
            }))
        }
        // AssistantTextEvent, ThinkingDeltaEvent, etc. — skip
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Streaming a single response
// ---------------------------------------------------------------------------

async fn stream_response(base_url: &str, token: &str, agent_id: &str, prompt: &str) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<StreamEvent>();

    let base_url = base_url.to_string();
    let token = token.to_string();
    let agent_id = agent_id.to_string();
    let prompt = prompt.to_string();

    let handle = tokio::spawn(async move {
        let result = crate::graphql::subscribe_agent_message(
            &base_url,
            &token,
            &agent_id,
            &prompt,
            |event| {
                if let Some(evt) = parse_stream_event(&event) {
                    tx.send(evt).is_ok()
                } else {
                    true
                }
            },
        )
        .await;

        if let Err(e) = result {
            // If channel is closed, the receiver already dropped — that's fine
            let _ = tx.send(StreamEvent::Error(e.to_string()));
        }
    });

    let mut needs_newline = false;

    loop {
        tokio::select! {
            event = rx.recv() => {
                let Some(event) = event else { break };
                match event {
                    StreamEvent::Delta(text) => {
                        print!("{text}");
                        std::io::stdout().flush().ok();
                        needs_newline = !text.ends_with('\n');
                    }
                    StreamEvent::ToolStart(name) => {
                        if needs_newline { println!(); needs_newline = false; }
                        println!("\x1b[2m[tool] calling {name}...\x1b[0m");
                    }
                    StreamEvent::ToolResult { name, is_error } => {
                        let status = if is_error { "error" } else { "done" };
                        println!("\x1b[2m[tool] {name} {status}\x1b[0m");
                    }
                    StreamEvent::Service(msg) => {
                        if needs_newline { println!(); needs_newline = false; }
                        println!("\x1b[2m[service] {msg}\x1b[0m");
                    }
                    StreamEvent::Error(msg) => {
                        if needs_newline { println!(); needs_newline = false; }
                        eprintln!("\x1b[31m[error] {msg}\x1b[0m");
                    }
                    StreamEvent::Done(usage) => {
                        if needs_newline { println!(); }
                        let total = usage.input_tokens + usage.output_tokens;
                        let mut parts = vec![
                            format!("{} turn{}", usage.turns, if usage.turns != 1 { "s" } else { "" }),
                            format!("{total} tokens"),
                        ];
                        if let Some(ms) = usage.duration_ms {
                            parts.push(format!("{:.1}s", ms as f64 / 1000.0));
                        }
                        if let Some(cost) = usage.cost_usd {
                            parts.push(format!("${cost:.4}"));
                        }
                        println!("\x1b[2m[{}]\x1b[0m", parts.join(" | "));
                        break;
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                handle.abort();
                if needs_newline { println!(); }
                eprintln!("\x1b[2m[interrupted]\x1b[0m");
                break;
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// REPL
// ---------------------------------------------------------------------------

pub async fn run(agent_id: Option<String>) -> Result<()> {
    let mut config = Config::load()?;
    let client = GraphqlClient::new(&config.server_url, &config.auth_token);

    let agent_id = ensure_agent(&mut config, &client, agent_id).await?;
    eprintln!("Agent: {agent_id}");
    eprintln!("Type /exit to quit, Ctrl-C to interrupt.\n");

    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();

    loop {
        print!("> ");
        std::io::stdout().flush().ok();

        let line = tokio::select! {
            result = lines.next_line() => {
                match result? {
                    Some(line) => line,
                    None => break, // EOF
                }
            }
            _ = tokio::signal::ctrl_c() => {
                println!();
                break;
            }
        };

        let input = line.trim();
        if input.is_empty() {
            continue;
        }

        match input {
            "/exit" | "/quit" => break,
            _ => {}
        }

        if let Err(e) = stream_response(&config.server_url, &config.auth_token, &agent_id, input)
            .await
        {
            eprintln!("\x1b[31m[error] {e}\x1b[0m");
        }
        println!();
    }

    Ok(())
}
