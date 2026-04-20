use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{Event, EventStream, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use ratatui::{backend::CrosstermBackend, Terminal};
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::graphql::GraphqlClient;
use crate::tui::chat::{ChatMessage, ChatRole, ContentBlock};
use crate::tui::state::{AgentItem, SandboxInfo, ScreenState, WorkspaceItem};
use crate::tui::{handlers, ui};

// ---------------------------------------------------------------------------
// GraphQL response types
// ---------------------------------------------------------------------------

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
    #[serde(default)]
    agents: Vec<AgentNode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentNode {
    id: String,
    name: String,
    role: String,
    attached_sandbox: Option<SandboxAttachmentNode>,
}

#[derive(Debug, Deserialize)]
struct SandboxAttachmentNode {
    name: String,
    mode: String,
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
                        attachedSandbox { name mode }
                    }
                    agents {
                        id
                        name
                        role
                        attachedSandbox { name mode }
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

// ---------------------------------------------------------------------------
// Chat history (GQL)
// ---------------------------------------------------------------------------

const CHAT_HISTORY_QUERY: &str = r#"
    query ChatHistory($agentId: AgentId!) {
        agent(id: $agentId) {
            session {
                chatHistory(last: 50) {
                    sequence
                    role
                    content {
                        __typename
                        ... on TextContent { text }
                        ... on ToolUseContent { name }
                        ... on ThinkingContent { text }
                        ... on ToolResultContent { toolUseId content isError }
                        ... on SandboxNotificationContent { sandboxName operation }
                    }
                }
            }
        }
    }
"#;

#[derive(Debug, Deserialize)]
struct ChatHistoryResponse {
    agent: Option<ChatHistoryAgent>,
}

#[derive(Debug, Deserialize)]
struct ChatHistoryAgent {
    session: ChatHistorySession,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChatHistorySession {
    chat_history: Vec<ChatHistoryMessage>,
}

#[derive(Debug, Deserialize)]
struct ChatHistoryMessage {
    role: String,
    content: Vec<ChatHistoryContentBlock>,
    #[allow(dead_code)]
    sequence: i32,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "__typename")]
enum ChatHistoryContentBlock {
    TextContent {
        text: String,
    },
    ToolUseContent {
        name: String,
    },
    ThinkingContent {
        text: String,
    },
    ToolResultContent {
        #[serde(rename = "toolUseId")]
        #[allow(dead_code)]
        tool_use_id: String,
        content: String,
        #[serde(rename = "isError")]
        is_error: bool,
    },
    SandboxNotificationContent {
        #[serde(rename = "sandboxName")]
        sandbox_name: String,
        #[allow(dead_code)]
        operation: String,
    },
}

impl ChatHistoryContentBlock {
    fn into_content_block(self) -> ContentBlock {
        match self {
            Self::TextContent { text } => ContentBlock::Text(text),
            Self::ToolUseContent { name } => ContentBlock::ToolUse(name),
            Self::ThinkingContent { text } => ContentBlock::Thinking(text),
            Self::ToolResultContent {
                content, is_error, ..
            } => {
                let label = if is_error {
                    format!("[error] {content}")
                } else {
                    content
                };
                ContentBlock::ToolResult(label)
            }
            Self::SandboxNotificationContent {
                sandbox_name,
                operation,
            } => ContentBlock::Text(format!("[sandbox: {sandbox_name} — {operation}]")),
        }
    }
}

// ---------------------------------------------------------------------------
// Chat streaming
// ---------------------------------------------------------------------------

enum ChatStreamEvent {
    Delta(String),
    ToolUse(String),
    ToolResult(String),
    Error(String),
    Done,
    /// Pre-fetched chat history for an agent (agent_id, messages).
    HistoryLoaded(String, Vec<ChatMessage>),
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

// ---------------------------------------------------------------------------
// Data fetching
// ---------------------------------------------------------------------------

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

fn agent_node_to_item(a: AgentNode) -> AgentItem {
    AgentItem {
        id: a.id,
        name: a.name,
        role: a.role,
        sandbox: a.attached_sandbox.map(|s| SandboxInfo {
            name: s.name,
            mode: s.mode,
        }),
    }
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
                lead: node.lead.map(agent_node_to_item),
                agents: node.agents.into_iter().map(agent_node_to_item).collect(),
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

async fn fetch_chat_history(client: &GraphqlClient, agent_id: &str) -> Result<Vec<ChatMessage>> {
    let resp: ChatHistoryResponse = client
        .query(
            CHAT_HISTORY_QUERY,
            serde_json::json!({ "agentId": agent_id }),
        )
        .await?;

    let messages = resp
        .agent
        .map(|a| a.session.chat_history)
        .unwrap_or_default();

    Ok(messages
        .into_iter()
        .map(|m| {
            let role = match m.role.as_str() {
                "USER" => ChatRole::User,
                _ => ChatRole::Assistant,
            };
            let blocks = m
                .content
                .into_iter()
                .map(ChatHistoryContentBlock::into_content_block)
                .collect();
            ChatMessage { role, blocks }
        })
        .collect())
}

fn spawn_chat_history_fetch(
    base_url: String,
    token: String,
    agent_id: String,
    tx: mpsc::UnboundedSender<ChatStreamEvent>,
) {
    tokio::spawn(async move {
        let client = GraphqlClient::new(&base_url, &token);
        match fetch_chat_history(&client, &agent_id).await {
            Ok(messages) => {
                let _ = tx.send(ChatStreamEvent::HistoryLoaded(agent_id, messages));
            }
            Err(e) => {
                let _ = tx.send(ChatStreamEvent::Error(format!(
                    "Failed to load history: {e}"
                )));
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Dispatch stream events → AssistantChat
// ---------------------------------------------------------------------------

fn dispatch_stream_event(state: &mut ScreenState, evt: ChatStreamEvent) {
    match evt {
        ChatStreamEvent::Delta(text) => {
            state.chat_view.assistant.append_text(&text);
        }
        ChatStreamEvent::ToolUse(name) => {
            state
                .chat_view
                .assistant
                .add_tool_activity(format!("calling {name}…"));
        }
        ChatStreamEvent::ToolResult(summary) => {
            state.chat_view.assistant.add_tool_activity(summary);
        }
        ChatStreamEvent::Error(msg) => {
            state.chat_view.assistant.add_error(msg);
        }
        ChatStreamEvent::Done => {
            state.chat_view.assistant.finish_streaming();
        }
        ChatStreamEvent::HistoryLoaded(agent_id, messages) => {
            // Only apply if the agent is still the one we're looking at
            if state.selected_agent_id().as_deref() == Some(agent_id.as_str()) {
                state.chat_view.assistant.load_history(messages);
                state.chat_view.reset_scroll();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Auth bootstrap — delegate to login flow when credentials are missing/stale
// ---------------------------------------------------------------------------

async fn ensure_authenticated(server: Option<String>) -> Result<(Config, GraphqlClient, String)> {
    // Try existing config first
    if let Ok(config) = Config::load() {
        let client = GraphqlClient::new(&config.server_url, &config.auth_token);
        if let Ok(user_name) = fetch_user_name(&client).await {
            return Ok((config, client, user_name));
        }
        println!("Session expired — starting login flow…");
        println!();
    } else {
        println!("Not logged in — starting login flow…");
        println!();
    }

    super::login::run(server).await?;
    let config = Config::load()?;
    let client = GraphqlClient::new(&config.server_url, &config.auth_token);
    let user_name = fetch_user_name(&client).await?;
    Ok((config, client, user_name))
}

// ---------------------------------------------------------------------------
// Entry point & event loop
// ---------------------------------------------------------------------------

pub async fn run(server: Option<String>) -> Result<()> {
    let (config, client, user_name) = ensure_authenticated(server).await?;
    let workspaces = fetch_workspaces(&client).await?;

    let mut state = ScreenState::new(workspaces, config.server_url.clone(), user_name);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_event_loop(&mut terminal, &mut state, &client, &config).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut ScreenState,
    client: &GraphqlClient,
    config: &Config,
) -> Result<()> {
    let (stream_tx, mut stream_rx) = mpsc::unbounded_channel::<ChatStreamEvent>();
    let mut event_stream = EventStream::new();
    loop {
        terminal.draw(|frame| ui::draw(frame, state))?;

        if state.should_quit {
            break;
        }

        let timeout = if state.chat_view.assistant.streaming {
            Duration::from_millis(50)
        } else {
            Duration::from_millis(500)
        };

        tokio::select! {
            event = event_stream.next() => {
                if let Some(Ok(Event::Key(key))) = event {
                    if key.kind == KeyEventKind::Press {
                        match handlers::handle_key(state, key) {
                            handlers::Action::Quit => state.should_quit = true,
                            handlers::Action::Suspend => {
                                disable_raw_mode()?;
                                execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
                                terminal.show_cursor()?;
                                unsafe { libc::raise(libc::SIGTSTP); }
                                enable_raw_mode()?;
                                execute!(terminal.backend_mut(), EnterAlternateScreen)?;
                                terminal.clear()?;
                            }
                            handlers::Action::Refresh => {
                                if let Ok(workspaces) = fetch_workspaces(client).await {
                                    state.replace_workspaces(workspaces);
                                }
                            }
                            handlers::Action::CreateWorkspace { name, description } => {
                                match create_workspace(client, &name, &description).await {
                                    Ok(ws_name) => {
                                        state.exit_create_mode();
                                        if let Ok(workspaces) = fetch_workspaces(client).await {
                                            state.replace_workspaces(workspaces);
                                        }
                                        state.status_message =
                                            Some(format!("Created workspace: {ws_name}"));
                                    }
                                    Err(e) => {
                                        state.status_message = Some(format!("Error: {e}"));
                                    }
                                }
                            }
                            handlers::Action::SendChat { agent_id, prompt } => {
                                spawn_chat_stream(
                                    config.server_url.clone(),
                                    config.auth_token.clone(),
                                    agent_id,
                                    prompt,
                                    stream_tx.clone(),
                                );
                            }
                            handlers::Action::None => {}
                        }
                    }
                }
            }
            event = stream_rx.recv() => {
                if let Some(evt) = event {
                    dispatch_stream_event(state, evt);
                }
            }
            _ = tokio::time::sleep(timeout) => {}
        }

        // ── Reactive history load ────────────────────────────────────
        // When the selected agent changes (and we're not streaming), fetch history.
        let current_agent = state.selected_agent_id();
        if current_agent != state.loaded_agent_id && !state.chat_view.assistant.streaming {
            state.loaded_agent_id = current_agent.clone();
            state.chat_view.assistant.clear();
            state.chat_view.reset_scroll();
            if let Some(agent_id) = current_agent {
                spawn_chat_history_fetch(
                    config.server_url.clone(),
                    config.auth_token.clone(),
                    agent_id,
                    stream_tx.clone(),
                );
            }
        }
    }
    Ok(())
}
