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
    workspace: CreatedWorkspaceNode,
}

#[derive(Debug, Deserialize)]
struct CreatedWorkspaceNode {
    id: String,
}

#[derive(Debug, Deserialize)]
struct AgentCreateResponse {
    #[serde(rename = "agentCreate")]
    agent_create: AgentCreatePayload,
}

#[derive(Debug, Deserialize)]
struct AgentCreatePayload {
    agent: AgentIdOnly,
}

async fn provision_chat_agent(client: &GraphqlClient) -> Result<String> {
    // 1. Create workspace
    let ws_query = r#"
        mutation WorkspaceCreate($input: WorkspaceCreateInput!) {
            workspaceCreate(input: $input) {
                workspace { id }
            }
        }
    "#;
    let ws_input = serde_json::json!({
        "name": "drua-chat",
        "description": "Auto-provisioned workspace for drua chat"
    });
    let ws_resp: WorkspaceCreateResponse = client
        .query(ws_query, serde_json::json!({ "input": ws_input }))
        .await?;
    let workspace_id = &ws_resp.workspace_create.workspace.id;

    // 2. Create agent (not the lead — so we can attach sandboxes)
    let agent_query = r#"
        mutation AgentCreate($input: AgentCreateInput!) {
            agentCreate(input: $input) {
                agent { id }
            }
        }
    "#;
    let agent_input = serde_json::json!({
        "workspaceId": workspace_id,
        "name": "chat"
    });
    let agent_resp: AgentCreateResponse = client
        .query(agent_query, serde_json::json!({ "input": agent_input }))
        .await?;
    Ok(agent_resp.agent_create.agent.id)
}

async fn ensure_agent(config: &mut Config, explicit_agent: Option<String>) -> Result<String> {
    if let Some(id) = explicit_agent {
        return Ok(id);
    }

    if let Some(ref id) = config.chat_agent_id {
        let client = GraphqlClient::new(&config.server_url, &config.auth_token);
        match verify_agent(&client, id).await {
            Ok(true) => return Ok(id.clone()),
            Ok(false) => {
                eprintln!("Stored agent no longer exists, re-authenticating...");
            }
            Err(_) => {
                eprintln!("Session expired, re-authenticating...");
            }
        }
        // Token may be stale — force re-auth before provisioning
        config.chat_agent_id = None;
        *config = Config::load_or_dev_login_fresh(&config.server_url).await?;
    }

    let client = GraphqlClient::new(&config.server_url, &config.auth_token);
    eprintln!("Provisioning chat agent...");
    let agent_id = provision_chat_agent(&client).await?;
    config.chat_agent_id = Some(agent_id.clone());
    config.save()?;
    Ok(agent_id)
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

fn fmt_tokens(n: u32) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
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
    cache_read_input_tokens: u32,
    cache_creation_input_tokens: u32,
}

fn parse_stream_event(event: &serde_json::Value) -> Option<StreamEvent> {
    let typename = event.get("__typename")?.as_str()?;
    match typename {
        "TextDeltaEvent" => {
            let text = event.get("text")?.as_str()?;
            Some(StreamEvent::Delta(text.to_string()))
        }
        "ToolCallStartEvent" => {
            let name = event.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
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
            let u64_field = |name: &str| -> u32 {
                event.get(name).and_then(|v| v.as_u64()).unwrap_or(0) as u32
            };
            Some(StreamEvent::Done(UsageInfo {
                turns: u64_field("turns"),
                input_tokens: u64_field("inputTokens"),
                output_tokens: u64_field("outputTokens"),
                cache_read_input_tokens: u64_field("cacheReadInputTokens"),
                cache_creation_input_tokens: u64_field("cacheCreationInputTokens"),
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
                        let mut parts = Vec::new();
                        if usage.turns > 1 {
                            parts.push(format!("{} turns", usage.turns));
                        }
                        parts.push(format!("↑{}", fmt_tokens(usage.input_tokens)));
                        parts.push(format!("↓{}", fmt_tokens(usage.output_tokens)));
                        if usage.cache_read_input_tokens > 0 {
                            parts.push(format!(
                                "R{}",
                                fmt_tokens(usage.cache_read_input_tokens)
                            ));
                        }
                        if usage.cache_creation_input_tokens > 0 {
                            parts.push(format!(
                                "W{}",
                                fmt_tokens(usage.cache_creation_input_tokens)
                            ));
                        }
                        println!("\x1b[2m{}\x1b[0m", parts.join(" "));
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

pub async fn run(server: Option<String>, agent_id: Option<String>) -> Result<()> {
    let mut config = Config::load_or_dev_login(server).await?;

    let agent_id = ensure_agent(&mut config, agent_id).await?;
    eprintln!("Agent: {agent_id}");
    eprintln!("Type /exit to quit, Ctrl-C to interrupt.\n");

    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();

    loop {
        print!("> ");
        std::io::stdout().flush().ok();

        let line = tokio::select! {
            result = lines.next_line() => {
                match result {
                    Ok(Some(line)) => line,
                    Ok(None) => break,        // EOF
                    Err(_) => break,           // stdin error (e.g. EINTR from Ctrl-C)
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

        if let Err(e) =
            stream_response(&config.server_url, &config.auth_token, &agent_id, input).await
        {
            eprintln!("\x1b[31m[error] {e}\x1b[0m");
        }
        println!();
    }

    Ok(())
}
