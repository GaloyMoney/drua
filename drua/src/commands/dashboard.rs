use std::io;

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::graphql::GraphqlClient;
use crate::tui::app::{AgentItem, App, Focus, Mode, WorkspaceItem};
use crate::tui::ui;

#[derive(Debug, Deserialize)]
struct MeResponse {
    me: Option<MeUser>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MeUser {
    github_username: Option<String>,
    name: Option<String>,
    email: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WorkspacesResponse {
    workspaces: WorkspaceConnection,
}

#[derive(Debug, Deserialize)]
struct WorkspaceConnection {
    edges: Vec<WorkspaceEdge>,
}

#[derive(Debug, Deserialize)]
struct WorkspaceEdge {
    node: WorkspaceNode,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceNode {
    id: String,
    name: String,
    description: Option<String>,
    created_at: Option<String>,
    lead: Option<AgentNode>,
}

#[derive(Debug, Deserialize)]
struct AgentNode {
    id: String,
    name: String,
    role: String,
}

const WORKSPACES_QUERY: &str = r#"
    query {
        workspaces(first: 50) {
            edges {
                node {
                    id
                    name
                    description
                    createdAt
                    lead {
                        id
                        name
                        role
                    }
                }
            }
        }
    }
"#;

const WORKSPACE_CREATE_MUTATION: &str = r#"
    mutation WorkspaceCreate($input: WorkspaceCreateInput!) {
        workspaceCreate(input: $input) {
            workspace {
                id
                name
            }
        }
    }
"#;

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
    name: String,
}

enum ChatStreamEvent {
    Delta(String),
    ToolUse(String),
    ToolResult(String),
    Error(String),
    Done,
}

async fn create_workspace(client: &GraphqlClient, name: &str, description: &str) -> Result<String> {
    let mut input = serde_json::json!({ "name": name });
    if !description.is_empty() {
        input["description"] = serde_json::json!(description);
    }
    let resp: WorkspaceCreateResponse = client
        .query(
            WORKSPACE_CREATE_MUTATION,
            serde_json::json!({ "input": input }),
        )
        .await?;
    Ok(resp.workspace_create.workspace.name)
}

async fn fetch_workspaces(client: &GraphqlClient) -> Result<Vec<WorkspaceItem>> {
    let resp: WorkspacesResponse = client
        .query(WORKSPACES_QUERY, serde_json::json!({}))
        .await?;

    let items = resp
        .workspaces
        .edges
        .into_iter()
        .map(|edge| {
            let node = edge.node;
            WorkspaceItem {
                id: node.id,
                name: node.name,
                description: node.description,
                created_at: node.created_at,
                lead: node.lead.map(|a| AgentItem {
                    id: a.id,
                    name: a.name,
                    role: a.role,
                }),
            }
        })
        .collect();

    Ok(items)
}

async fn fetch_user_name(client: &GraphqlClient) -> Result<String> {
    let resp: MeResponse = client
        .query(
            "{ me { githubUsername name email } }",
            serde_json::json!({}),
        )
        .await?;
    let user = resp
        .me
        .ok_or_else(|| anyhow::anyhow!("not authenticated"))?;
    Ok(user
        .github_username
        .or(user.name)
        .or(user.email)
        .unwrap_or_else(|| "unknown".to_string()))
}

fn spawn_chat_stream(
    base_url: String,
    token: String,
    agent_id: String,
    prompt: String,
    tx: mpsc::UnboundedSender<ChatStreamEvent>,
) {
    tokio::spawn(async move {
        if let Err(e) = stream_chat_response(&base_url, &token, &agent_id, &prompt, &tx).await {
            let _ = tx.send(ChatStreamEvent::Error(e.to_string()));
        }
        let _ = tx.send(ChatStreamEvent::Done);
    });
}

async fn stream_chat_response(
    base_url: &str,
    token: &str,
    agent_id: &str,
    prompt: &str,
    tx: &mpsc::UnboundedSender<ChatStreamEvent>,
) -> Result<()> {
    let http = reqwest::Client::new();
    let url = format!("{base_url}/api/v1/agents/{agent_id}/message");

    let resp = http
        .post(&url)
        .bearer_auth(token)
        .json(&serde_json::json!({ "prompt": prompt }))
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("server returned {status}: {text}");
    }

    let mut buf = String::new();
    let mut response = resp;

    while let Some(chunk) = response.chunk().await? {
        let text = String::from_utf8_lossy(&chunk);
        buf.push_str(&text);

        while let Some(end) = buf.find("\n\n") {
            let block = buf[..end].to_string();
            buf = buf[end + 2..].to_string();
            if let Some(evt) = parse_sse_block(&block) {
                if tx.send(evt).is_err() {
                    return Ok(());
                }
            }
        }
    }

    // Process any remaining data
    if !buf.trim().is_empty() {
        if let Some(evt) = parse_sse_block(&buf) {
            let _ = tx.send(evt);
        }
    }

    Ok(())
}

fn parse_sse_block(block: &str) -> Option<ChatStreamEvent> {
    let mut event_type = String::new();
    let mut data = String::new();

    for line in block.lines() {
        if let Some(rest) = line.strip_prefix("event: ") {
            event_type = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data: ") {
            data = rest.trim().to_string();
        }
    }

    if data.is_empty() {
        return None;
    }

    let json: serde_json::Value = serde_json::from_str(&data).ok()?;

    match event_type.as_str() {
        "text_delta" => {
            let text = json.get("text")?.as_str()?;
            Some(ChatStreamEvent::Delta(text.to_string()))
        }
        // assistant_text is the complete message — skip if we already got deltas
        "assistant_text" => None,
        "tool_call_start" => {
            let name = json.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
            Some(ChatStreamEvent::ToolUse(name.to_string()))
        }
        "tool_result" => {
            let name = json.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
            let is_error = json
                .get("is_error")
                .and_then(|e| e.as_bool())
                .unwrap_or(false);
            let label = if is_error {
                format!("{name} (error)")
            } else {
                format!("{name} (done)")
            };
            Some(ChatStreamEvent::ToolResult(label))
        }
        "error" => {
            let msg = json
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            Some(ChatStreamEvent::Error(msg.to_string()))
        }
        "assistant_done" => Some(ChatStreamEvent::Done),
        // Ignore events we don't need to display
        "user_message"
        | "thinking"
        | "thinking_delta"
        | "tool_call"
        | "tool_call_input_delta"
        | "service" => None,
        _ => None,
    }
}

pub async fn run() -> Result<()> {
    let config = Config::load()?;
    let client = GraphqlClient::new(&config.server_url, &config.auth_token);

    let user_name = fetch_user_name(&client).await?;
    let workspaces = fetch_workspaces(&client).await?;

    let mut app = App::new(workspaces, config.server_url.clone(), user_name);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_event_loop(&mut terminal, &mut app, &client, &config).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    client: &GraphqlClient,
    config: &Config,
) -> Result<()> {
    let (stream_tx, mut stream_rx) = mpsc::unbounded_channel::<ChatStreamEvent>();

    loop {
        // Drain pending stream events
        while let Ok(evt) = stream_rx.try_recv() {
            match evt {
                ChatStreamEvent::Delta(text) => {
                    app.append_to_last_assistant(&text);
                }
                ChatStreamEvent::ToolUse(name) => {
                    app.push_chat_message("tool", format!("calling {name}…"));
                }
                ChatStreamEvent::ToolResult(summary) => {
                    app.push_chat_message("tool", summary);
                }
                ChatStreamEvent::Error(msg) => {
                    app.push_chat_message("system", format!("Error: {msg}"));
                    app.chat_streaming = false;
                }
                ChatStreamEvent::Done => {
                    app.chat_streaming = false;
                }
            }
        }

        terminal.draw(|frame| ui::draw(frame, app))?;

        if app.should_quit {
            break;
        }

        if event::poll(std::time::Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                    app.should_quit = true;
                    continue;
                }

                match app.mode {
                    Mode::Browse => match app.focus {
                        Focus::Sidebar => {
                            app.status_message = None;
                            match key.code {
                                KeyCode::Char('q') => app.should_quit = true,
                                KeyCode::Char('j') | KeyCode::Down => app.cursor_down(),
                                KeyCode::Char('k') | KeyCode::Up => app.cursor_up(),
                                KeyCode::Char('n') => app.enter_create_mode(),
                                KeyCode::Char('r') => {
                                    if let Ok(workspaces) = fetch_workspaces(client).await {
                                        app.replace_workspaces(workspaces);
                                    }
                                }
                                KeyCode::Tab => app.toggle_focus(),
                                _ => {}
                            }
                        }
                        Focus::Chat => match key.code {
                            KeyCode::Esc => {
                                app.focus = Focus::Sidebar;
                            }
                            KeyCode::Tab => app.toggle_focus(),
                            KeyCode::Enter => {
                                let input = app.chat_input.trim().to_string();
                                if !input.is_empty() {
                                    if let Some(agent_id) = app.selected_lead_id.clone() {
                                        app.push_chat_message("user", &input);
                                        app.chat_input.clear();
                                        app.chat_streaming = true;
                                        spawn_chat_stream(
                                            config.server_url.clone(),
                                            config.auth_token.clone(),
                                            agent_id,
                                            input,
                                            stream_tx.clone(),
                                        );
                                    } else {
                                        app.status_message =
                                            Some("No lead agent selected".to_string());
                                    }
                                }
                            }
                            KeyCode::Backspace => {
                                app.chat_input.pop();
                            }
                            KeyCode::Up => {
                                app.chat_scroll = app.chat_scroll.saturating_add(1);
                            }
                            KeyCode::Down => {
                                app.chat_scroll = app.chat_scroll.saturating_sub(1);
                            }
                            KeyCode::Char(c) => {
                                app.chat_input.push(c);
                            }
                            _ => {}
                        },
                    },
                    Mode::CreateWorkspace => match key.code {
                        KeyCode::Esc => app.exit_create_mode(),
                        KeyCode::Tab | KeyCode::BackTab => {
                            app.input_field = (app.input_field + 1) % 2;
                        }
                        KeyCode::Backspace => {
                            app.active_input_mut().pop();
                        }
                        KeyCode::Char(c) => {
                            app.active_input_mut().push(c);
                        }
                        KeyCode::Enter => {
                            if app.input_name.trim().is_empty() {
                                app.status_message = Some("Name is required".to_string());
                                continue;
                            }
                            let name = app.input_name.trim().to_string();
                            let desc = app.input_description.trim().to_string();
                            match create_workspace(client, &name, &desc).await {
                                Ok(ws_name) => {
                                    app.exit_create_mode();
                                    if let Ok(workspaces) = fetch_workspaces(client).await {
                                        app.replace_workspaces(workspaces);
                                    }
                                    app.status_message =
                                        Some(format!("Created workspace: {ws_name}"));
                                }
                                Err(e) => {
                                    app.status_message = Some(format!("Error: {e}"));
                                }
                            }
                        }
                        _ => {}
                    },
                }
            }
        }
    }
    Ok(())
}
