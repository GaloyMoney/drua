use std::collections::{BTreeSet, HashMap};
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
use crate::tui::state::{
    AgentItem, BlockDetail, CellKind, Focus, GridSection, SandboxInfo, ScreenState,
    SystemBlockDetail, ThreadGridState, ThreadInfo, UsageDetail, WorkspaceItem,
};
use crate::tui::{handlers, ui};

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
    model: String,
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
                        model
                        attachedSandbox { name mode }
                    }
                    agents {
                        id
                        name
                        role
                        model
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
                        ... on ToolUseContent { name input }
                        ... on ThinkingContent { text }
                        ... on ToolResultContent { toolUseId content isError }
                        ... on SandboxNotificationContent { sandboxName operation mode text }
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
#[allow(clippy::enum_variant_names)]
enum ChatHistoryContentBlock {
    TextContent {
        text: String,
    },
    ToolUseContent {
        name: String,
        #[serde(default)]
        input: String,
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
        operation: String,
        #[serde(default)]
        mode: Option<String>,
        text: String,
    },
}

impl ChatHistoryContentBlock {
    fn into_content_block(self) -> ContentBlock {
        match self {
            Self::TextContent { text } => ContentBlock::Text(text),
            Self::ToolUseContent { name, input } => ContentBlock::ToolUse { name, input },
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
                mode,
                text,
            } => ContentBlock::Sandbox {
                sandbox_name,
                operation,
                mode,
                text,
            },
        }
    }
}

const THREADS_QUERY: &str = r#"
    query Threads($agentId: AgentId!) {
        agent(id: $agentId) {
            session {
                threads {
                    id
                    isCurrent
                    nextTurn
                    startReason
                    systemBlocks {
                        index
                        kind
                        text
                    }
                    toolDefinitionsCount
                    messages {
                        role
                        blockIndexes
                        content {
                            __typename
                            ... on TextContent { text }
                            ... on ToolUseContent { name input }
                            ... on ThinkingContent { text }
                            ... on ToolResultContent { toolUseId content isError }
                            ... on SandboxNotificationContent { sandboxName operation mode text }
                        }
                        usage {
                            model
                            inputTokens
                            outputTokens
                            cacheReadTokens
                            cacheWriteTokens
                            totalTokens
                            totalCost
                        }
                        recordedAt
                    }
                }
            }
        }
    }
"#;

#[derive(Debug, Deserialize)]
struct ThreadsResponse {
    agent: Option<ThreadsAgent>,
}

#[derive(Debug, Deserialize)]
struct ThreadsAgent {
    session: ThreadsSession,
}

#[derive(Debug, Deserialize)]
struct ThreadsSession {
    threads: Vec<ThreadNode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadNode {
    id: String,
    is_current: bool,
    next_turn: String,
    start_reason: String,
    #[serde(default)]
    system_blocks: Vec<SystemBlockInfoNode>,
    #[serde(default)]
    tool_definitions_count: i32,
    messages: Vec<ThreadMessageNode>,
}

#[derive(Debug, Deserialize)]
struct SystemBlockInfoNode {
    index: i32,
    kind: String,
    #[serde(default)]
    text: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadMessageNode {
    role: String,
    block_indexes: Vec<i32>,
    content: Vec<ChatHistoryContentBlock>,
    usage: Option<ThreadMessageUsageNode>,
    recorded_at: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadMessageUsageNode {
    model: String,
    input_tokens: i32,
    output_tokens: i32,
    cache_read_tokens: i32,
    cache_write_tokens: i32,
    total_tokens: i32,
    total_cost: f64,
}

enum ChatStreamEvent {
    Delta(String),
    ToolUse(String),
    ToolResult(String),
    Error(String),
    Done,
    HistoryLoaded(String, Vec<ChatMessage>),
    ThreadsLoaded(String, Box<ThreadGridState>),
    ExportComplete(String),
}

fn spawn_chat_stream(
    base_url: String,
    token: String,
    agent_id: String,
    prompt: String,
    tx: mpsc::UnboundedSender<ChatStreamEvent>,
) {
    tokio::spawn(async move {
        let result = crate::graphql::subscribe_agent_message(
            &base_url,
            &token,
            &agent_id,
            &prompt,
            |event| {
                let evt = parse_gql_event(&event);
                if let Some(evt) = evt {
                    return tx.send(evt).is_ok();
                }
                true
            },
        )
        .await;

        if let Err(e) = result {
            let _ = tx.send(ChatStreamEvent::Error(e.to_string()));
        }
        let _ = tx.send(ChatStreamEvent::Done);
    });
}

fn parse_gql_event(event: &serde_json::Value) -> Option<ChatStreamEvent> {
    let typename = event.get("__typename")?.as_str()?;
    match typename {
        "AssistantTextDeltaEvent" => {
            let text = event.get("text")?.as_str()?;
            Some(ChatStreamEvent::Delta(text.to_string()))
        }
        // skip the complete message; we already got deltas
        "AssistantTextEvent" => None,
        "ToolCallStartEvent" => {
            let name = event.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
            Some(ChatStreamEvent::ToolUse(name.to_string()))
        }
        "ToolResultEvent" => {
            let name = event.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
            let is_error = event
                .get("isError")
                .and_then(|e| e.as_bool())
                .unwrap_or(false);
            let label = if is_error {
                format!("{name} (error)")
            } else {
                format!("{name} (done)")
            };
            Some(ChatStreamEvent::ToolResult(label))
        }
        "ErrorEvent" => {
            let msg = event
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            Some(ChatStreamEvent::Error(msg.to_string()))
        }
        "AssistantDoneEvent" => Some(ChatStreamEvent::Done),
        _ => None,
    }
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

fn agent_node_to_item(a: AgentNode) -> AgentItem {
    AgentItem {
        id: a.id,
        name: a.name,
        role: a.role,
        model: a.model,
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

async fn fetch_threads(client: &GraphqlClient, agent_id: &str) -> Result<ThreadGridState> {
    let resp: ThreadsResponse = client
        .query(THREADS_QUERY, serde_json::json!({ "agentId": agent_id }))
        .await?;

    let thread_nodes = resp.agent.map(|a| a.session.threads).unwrap_or_default();

    Ok(build_thread_grid(thread_nodes))
}

/// Columns = unique block indexes (sorted). Rows = threads.
/// First thread to reference a block index "owns" it (Unique).
/// Subsequent threads referencing the same index get Shared.
/// Unique blocks in COMPACTION threads are marked Summary.
fn build_thread_grid(thread_nodes: Vec<ThreadNode>) -> ThreadGridState {
    let mut all_positions = BTreeSet::new();
    let mut owner: HashMap<i32, usize> = HashMap::new();

    for (thread_idx, node) in thread_nodes.iter().enumerate() {
        for msg in &node.messages {
            for &bi in &msg.block_indexes {
                all_positions.insert(bi);
                owner.entry(bi).or_insert(thread_idx);
            }
        }
    }

    let mut all_system_positions = BTreeSet::new();
    let mut system_owner: HashMap<i32, usize> = HashMap::new();
    for (thread_idx, node) in thread_nodes.iter().enumerate() {
        for sb in &node.system_blocks {
            all_system_positions.insert(sb.index);
            system_owner.entry(sb.index).or_insert(thread_idx);
        }
    }
    let system_positions: Vec<i32> = all_system_positions.into_iter().collect();
    let sys_pos_map: HashMap<i32, usize> = system_positions
        .iter()
        .enumerate()
        .map(|(i, &pos)| (pos, i))
        .collect();
    let num_system_positions = system_positions.len();

    let positions: Vec<i32> = all_positions.into_iter().collect();
    let pos_map: HashMap<i32, usize> = positions
        .iter()
        .enumerate()
        .map(|(i, &pos)| (pos, i))
        .collect();

    let num_positions = positions.len();
    let num_threads = thread_nodes.len();

    let mut grid: Vec<Vec<CellKind>> = vec![vec![CellKind::Empty; num_positions]; num_threads];
    let mut system_grid: Vec<Vec<CellKind>> =
        vec![vec![CellKind::Empty; num_system_positions]; num_threads];
    let mut tool_def_counts: Vec<usize> = vec![0; num_threads];
    let mut system_details: HashMap<(usize, usize), SystemBlockDetail> = HashMap::new();
    let mut details: HashMap<(usize, usize), BlockDetail> = HashMap::new();
    let mut thread_infos = Vec::new();
    // Owner's content per block-index, for condensed detection.
    let mut owner_content: HashMap<i32, ContentBlock> = HashMap::new();

    for (thread_idx, node) in thread_nodes.into_iter().enumerate() {
        let is_compaction = node.start_reason == "COMPACTION";
        let msg_count = node.messages.len();

        // Owners get Unique(<kind letter>), subsequent referencers get Shared.
        for sb in &node.system_blocks {
            if let Some(&col) = sys_pos_map.get(&sb.index) {
                let is_owner = system_owner.get(&sb.index) == Some(&thread_idx);
                let cell = if is_owner {
                    CellKind::Unique(system_kind_letter(&sb.kind))
                } else {
                    CellKind::Shared
                };
                system_grid[thread_idx][col] = cell;
                system_details.insert(
                    (thread_idx, col),
                    SystemBlockDetail {
                        kind: sb.kind.clone(),
                        text: sb.text.clone(),
                    },
                );
            }
        }
        tool_def_counts[thread_idx] = node.tool_definitions_count.max(0) as usize;

        thread_infos.push(ThreadInfo {
            id: node.id,
            is_current: node.is_current,
            next_turn: node.next_turn,
            start_reason: node.start_reason,
            message_count: msg_count,
        });

        for msg in node.messages {
            let role = match msg.role.as_str() {
                "USER" => ChatRole::User,
                _ => ChatRole::Assistant,
            };

            // Shared by all blocks in this turn.
            let msg_usage = msg.usage.map(|u| UsageDetail {
                model: u.model,
                input_tokens: u.input_tokens,
                output_tokens: u.output_tokens,
                cache_read_tokens: u.cache_read_tokens,
                cache_write_tokens: u.cache_write_tokens,
                total_tokens: u.total_tokens,
                total_cost: u.total_cost,
            });
            let msg_recorded_at = msg.recorded_at;

            for (content_block, bi) in msg.content.into_iter().zip(msg.block_indexes) {
                let pos_idx = match pos_map.get(&bi) {
                    Some(&idx) => idx,
                    None => continue,
                };

                let content = content_block.into_content_block();
                let type_char = content_type_char(&content, role);
                let is_owner = owner.get(&bi) == Some(&thread_idx);

                let cell = if is_owner {
                    owner_content.insert(bi, content.clone());
                    if is_compaction {
                        CellKind::Summary(type_char)
                    } else {
                        CellKind::Unique(type_char)
                    }
                } else {
                    // Different from owner's content => condensed/masked.
                    match owner_content.get(&bi) {
                        Some(owner_c) if *owner_c != content => CellKind::Condensed,
                        _ => CellKind::Shared,
                    }
                };

                grid[thread_idx][pos_idx] = cell;
                details.insert(
                    (thread_idx, pos_idx),
                    BlockDetail {
                        role,
                        content,
                        usage: msg_usage.clone(),
                        recorded_at: msg_recorded_at.clone(),
                    },
                );
            }
        }
    }

    let current_thread_idx = thread_infos.iter().position(|t| t.is_current).unwrap_or(0);
    let initial_col = grid
        .get(current_thread_idx)
        .and_then(|row| {
            row.iter().position(|c| {
                matches!(
                    c,
                    CellKind::Unique(_) | CellKind::Summary(_) | CellKind::Condensed
                )
            })
        })
        .unwrap_or(0);

    ThreadGridState {
        threads: thread_infos,
        positions,
        grid,
        details,
        cursor_col: initial_col,
        cursor_row: current_thread_idx,
        scroll_col: 0,
        visible_cols: 0,
        system_positions,
        system_grid,
        tool_def_counts,
        cursor_section: GridSection::Messages,
        system_details,
    }
}

/// B=Base, T=Tools, H=beHavioral (avoids B clash), R=Role, N=Notes, S=Skills.
fn system_kind_letter(kind: &str) -> char {
    match kind {
        "BASE" => 'B',
        "TOOLS" => 'T',
        "BEHAVIORAL" => 'H',
        "ROLE" => 'R',
        "NOTES" => 'N',
        "SKILLS" => 'S',
        _ => '?',
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sys(index: i32, kind: &str) -> SystemBlockInfoNode {
        SystemBlockInfoNode {
            index,
            kind: kind.to_string(),
            text: String::new(),
        }
    }

    fn make_thread(
        id: &str,
        is_current: bool,
        start_reason: &str,
        system_blocks: Vec<SystemBlockInfoNode>,
        tool_count: i32,
    ) -> ThreadNode {
        ThreadNode {
            id: id.to_string(),
            is_current,
            next_turn: "USER".to_string(),
            start_reason: start_reason.to_string(),
            system_blocks,
            tool_definitions_count: tool_count,
            messages: Vec::new(),
        }
    }

    #[test]
    fn system_grid_positions_are_sorted_global_indexes() {
        let threads = vec![
            make_thread(
                "t1",
                true,
                "INITIAL_THREAD",
                vec![sys(0, "BASE"), sys(1, "TOOLS"), sys(4, "NOTES")],
                10,
            ),
            make_thread(
                "t2",
                false,
                "CONTEXT_REFRESHED",
                vec![sys(0, "BASE"), sys(1, "TOOLS"), sys(7, "NOTES")],
                12,
            ),
        ];

        let grid = build_thread_grid(threads);
        assert_eq!(grid.system_positions, vec![0, 1, 4, 7]);
    }

    #[test]
    fn first_thread_owns_shared_system_blocks() {
        let threads = vec![
            make_thread(
                "t1",
                true,
                "INITIAL_THREAD",
                vec![sys(0, "BASE"), sys(1, "TOOLS")],
                5,
            ),
            make_thread(
                "t2",
                false,
                "CONTEXT_REFRESHED",
                vec![sys(0, "BASE"), sys(1, "TOOLS"), sys(2, "NOTES")],
                5,
            ),
        ];

        let grid = build_thread_grid(threads);

        assert!(matches!(grid.system_grid[0][0], CellKind::Unique('B')));
        assert!(matches!(grid.system_grid[0][1], CellKind::Unique('T')));
        assert!(matches!(grid.system_grid[0][2], CellKind::Empty));
        assert!(matches!(grid.system_grid[1][0], CellKind::Shared));
        assert!(matches!(grid.system_grid[1][1], CellKind::Shared));
        assert!(matches!(grid.system_grid[1][2], CellKind::Unique('N')));
    }

    #[test]
    fn tool_def_counts_per_thread() {
        let threads = vec![
            make_thread("t1", true, "INITIAL_THREAD", vec![sys(0, "BASE")], 17),
            make_thread("t2", false, "TOOL_DEFS_UPDATED", vec![sys(0, "BASE")], 25),
        ];

        let grid = build_thread_grid(threads);
        assert_eq!(grid.tool_def_counts, vec![17, 25]);
    }

    #[test]
    fn cursor_section_defaults_to_messages() {
        let threads = vec![make_thread(
            "t1",
            true,
            "INITIAL_THREAD",
            vec![sys(0, "BASE")],
            5,
        )];
        let grid = build_thread_grid(threads);
        assert_eq!(grid.cursor_section, GridSection::Messages);
    }
}

fn content_type_char(content: &ContentBlock, role: ChatRole) -> char {
    match content {
        ContentBlock::Text(_) => match role {
            ChatRole::User => 'U',
            ChatRole::Assistant | ChatRole::System => 'A',
        },
        ContentBlock::ToolUse { .. } => 'T',
        ContentBlock::Thinking(_) => 't',
        ContentBlock::ToolResult(_) => 'R',
        ContentBlock::Sandbox { .. } => 'S',
    }
}

const EXPORT_THREAD_QUERY: &str = r#"
    query ExportThread($agentId: AgentId!) {
        exportThread(agentId: $agentId)
    }
"#;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExportThreadResponse {
    export_thread: String,
}

fn spawn_thread_export(
    base_url: String,
    token: String,
    agent_id: String,
    path: String,
    tx: mpsc::UnboundedSender<ChatStreamEvent>,
) {
    tokio::spawn(async move {
        let client = GraphqlClient::new(&base_url, &token);
        let result: Result<ExportThreadResponse> = client
            .query(
                EXPORT_THREAD_QUERY,
                serde_json::json!({ "agentId": agent_id }),
            )
            .await;
        match result {
            Ok(resp) => match std::fs::write(&path, &resp.export_thread) {
                Ok(()) => {
                    let _ = tx.send(ChatStreamEvent::ExportComplete(format!(
                        "Exported to {path}"
                    )));
                }
                Err(e) => {
                    let _ = tx.send(ChatStreamEvent::Error(format!(
                        "Failed to write {path}: {e}"
                    )));
                }
            },
            Err(e) => {
                let _ = tx.send(ChatStreamEvent::Error(format!("Export failed: {e}")));
            }
        }
    });
}

fn spawn_threads_fetch(
    base_url: String,
    token: String,
    agent_id: String,
    tx: mpsc::UnboundedSender<ChatStreamEvent>,
) {
    tokio::spawn(async move {
        let client = GraphqlClient::new(&base_url, &token);
        match fetch_threads(&client, &agent_id).await {
            Ok(grid) => {
                let _ = tx.send(ChatStreamEvent::ThreadsLoaded(agent_id, Box::new(grid)));
            }
            Err(e) => {
                let _ = tx.send(ChatStreamEvent::Error(format!(
                    "Failed to load threads: {e}"
                )));
            }
        }
    });
}

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
            if state.selected_agent_id().as_deref() == Some(agent_id.as_str()) {
                state.chat_view.assistant.load_history(messages);
                state.chat_view.reset_scroll();
            }
        }
        ChatStreamEvent::ThreadsLoaded(agent_id, grid) => {
            if state.selected_agent_id().as_deref() == Some(agent_id.as_str()) {
                state.status_message = None;
                state.thread_view = Some(*grid);
                state.focus = Focus::Threads;
            }
        }
        ChatStreamEvent::ExportComplete(msg) => {
            state.status_message = Some(msg);
        }
    }
}

/// Delegates to login flow when credentials are missing/stale.
async fn ensure_authenticated(server: Option<String>) -> Result<(Config, GraphqlClient, String)> {
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
                            handlers::Action::ExportThread { agent_id, path } => {
                                state.status_message = Some("Exporting…".to_string());
                                spawn_thread_export(
                                    config.server_url.clone(),
                                    config.auth_token.clone(),
                                    agent_id,
                                    path,
                                    stream_tx.clone(),
                                );
                            }
                            handlers::Action::ToggleThreads => {
                                if state.thread_view.is_some() {
                                    state.thread_view = None;
                                    state.focus = Focus::Chat;
                                    state.loaded_agent_id = None; // triggers reactive reload below
                                } else if let Some(agent_id) = state.selected_agent_id() {
                                    state.status_message = Some("Loading threads…".to_string());
                                    spawn_threads_fetch(
                                        config.server_url.clone(),
                                        config.auth_token.clone(),
                                        agent_id,
                                        stream_tx.clone(),
                                    );
                                }
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

        // Fetch history when the selected agent changes (and we're not streaming).
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
