use std::collections::{BTreeSet, HashMap};
use std::io;
use std::time::{Duration, Instant};

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
    AgentItem, BlockDetail, CellKind, Focus, GridSection, ProjectItem, SandboxInfo, ScreenState,
    SystemBlockDetail, ThreadGridState, ThreadInfo, UsageDetail, WorkflowDefinitionDetail,
    WorkflowDefinitionItem, WorkflowRunDetail, WorkflowRunItem, WorkflowSandboxItem,
    WorkflowStepItem, WorkflowStepResultItem, WorkflowTriggerInfo,
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
struct ProjectsResponse {
    projects: ProjectConnection,
}

#[derive(Debug, Deserialize)]
struct ProjectConnection {
    edges: Vec<ProjectEdge>,
}

#[derive(Debug, Deserialize)]
struct ProjectEdge {
    node: ProjectNode,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectNode {
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
    session: AgentSessionNode,
    attached_sandbox: Option<SandboxAttachmentNode>,
}

#[derive(Debug, Deserialize)]
struct AgentSessionNode {
    model: String,
}

#[derive(Debug, Deserialize)]
struct SandboxAttachmentNode {
    name: String,
    mode: String,
}

const WORKFLOW_DEFINITIONS_QUERY: &str = r#"
    query WorkflowDefinitions($projectId: ProjectId!) {
        workflowDefinitions(projectId: $projectId) {
            id
            name
            description
            trigger { kind provider cronSchedule cronTimezone nextRunAt condition webhookUrl }
            steps { name stepType skill tool sandbox sandboxMode condition }
            recentRuns(first: 1) {
                id
                state
                startedAt
                completedAt
                stepResults { name state output error skipped completedAt }
            }
        }
    }
"#;

const WORKFLOW_DEFINITION_QUERY: &str = r#"
    query WorkflowDefinition($id: WorkflowDefinitionId!) {
        workflowDefinition(id: $id) {
            id
            name
            description
            yaml
            trigger { kind provider cronSchedule cronTimezone nextRunAt condition webhookUrl }
            steps { name stepType skill tool sandbox sandboxMode condition }
            sandboxes { name kind branch repoUrl }
            recentRuns(first: 10) { id state startedAt completedAt stepResults { name state output error skipped completedAt } }
        }
    }
"#;

const WORKFLOW_RUNS_QUERY: &str = r#"
    query WorkflowRuns($definitionId: WorkflowDefinitionId!, $first: Int!) {
        workflowRuns(definitionId: $definitionId, first: $first) {
            id
            state
            startedAt
            completedAt
            stepResults { name state output error skipped completedAt }
        }
    }
"#;

const WORKFLOW_RUN_QUERY: &str = r#"
    query WorkflowRun($id: WorkflowRunId!) {
        workflowRun(id: $id) {
            id
            definitionId
            projectId
            state
            startedAt
            completedAt
            triggerContext
            stepsSnapshot { name stepType skill tool sandbox sandboxMode condition }
            stepResults { name state output error skipped completedAt }
            agents { id name role session { model } attachedSandbox { name mode } }
        }
    }
"#;

const WORKFLOW_TRIGGER_MUTATION: &str = r#"
    mutation WorkflowTrigger($input: WorkflowTriggerInput!) {
        workflowTrigger(input: $input) {
            filtered
            run {
                id
                definitionId
                projectId
                state
                startedAt
                completedAt
                stepResults { name state output error skipped completedAt }
            }
        }
    }
"#;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowDefinitionsResponse {
    workflow_definitions: Vec<WorkflowDefinitionNode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowDefinitionResponse {
    workflow_definition: Option<WorkflowDefinitionNode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowRunsResponse {
    workflow_runs: Vec<WorkflowRunNode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowRunResponse {
    workflow_run: Option<WorkflowRunNode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowTriggerResponse {
    workflow_trigger: WorkflowTriggerPayloadNode,
}

#[derive(Debug, Deserialize)]
struct WorkflowTriggerPayloadNode {
    filtered: bool,
    run: Option<WorkflowRunNode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowDefinitionNode {
    id: String,
    name: String,
    description: Option<String>,
    #[serde(default)]
    yaml: String,
    trigger: WorkflowTriggerNode,
    #[serde(default)]
    steps: Vec<WorkflowStepNode>,
    #[serde(default)]
    sandboxes: Vec<WorkflowSandboxNode>,
    #[serde(default)]
    recent_runs: Vec<WorkflowRunNode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowTriggerNode {
    kind: String,
    provider: Option<String>,
    cron_schedule: Option<String>,
    cron_timezone: Option<String>,
    next_run_at: Option<String>,
    condition: Option<String>,
    webhook_url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowStepNode {
    name: String,
    step_type: String,
    skill: Option<String>,
    tool: Option<String>,
    sandbox: Option<String>,
    sandbox_mode: Option<String>,
    condition: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowSandboxNode {
    name: String,
    kind: String,
    branch: Option<String>,
    repo_url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowRunNode {
    id: String,
    #[serde(default)]
    definition_id: String,
    #[serde(default)]
    project_id: String,
    state: String,
    started_at: String,
    completed_at: Option<String>,
    #[serde(default)]
    trigger_context: serde_json::Value,
    #[serde(default)]
    steps_snapshot: Vec<WorkflowStepNode>,
    #[serde(default)]
    step_results: Vec<WorkflowStepResultNode>,
    #[serde(default)]
    agents: Vec<AgentNode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowStepResultNode {
    name: String,
    state: String,
    output: Option<serde_json::Value>,
    error: Option<String>,
    skipped: Option<String>,
    completed_at: Option<String>,
}

const PROJECTS_QUERY: &str = r#"
    query {
        projects(first: 50) {
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
                        session { model }
                        attachedSandbox { name mode }
                    }
                    agents {
                        id
                        name
                        role
                        session { model }
                        attachedSandbox { name mode }
                    }
                }
            }
        }
    }
"#;

const BOOTSTRAP_MUTATION: &str = r#"
    mutation { bootstrapPersonalProject { project { id name } created } }
"#;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BootstrapResponse {
    bootstrap_personal_project: BootstrapPayload,
}

#[derive(Debug, Deserialize)]
struct BootstrapPayload {
    project: BootstrapProject,
    #[allow(dead_code)]
    created: bool,
}

#[derive(Debug, Deserialize)]
struct BootstrapProject {
    id: String,
    #[allow(dead_code)]
    name: String,
}

const PROJECT_CREATE_MUTATION: &str = r#"
    mutation ProjectCreate($input: ProjectCreateInput!) {
        projectCreate(input: $input) {
            project {
                id
                name
            }
        }
    }
"#;

#[derive(Debug, Deserialize)]
struct ProjectCreateResponse {
    #[serde(rename = "projectCreate")]
    project_create: ProjectCreatePayload,
}

#[derive(Debug, Deserialize)]
struct ProjectCreatePayload {
    project: CreatedProject,
}

#[derive(Debug, Deserialize)]
struct CreatedProject {
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
    HistoryLoaded(Vec<ChatMessage>),
    ThreadsLoaded(Box<ThreadGridState>),
    ExportComplete(String),
}

/// Every stream event is tagged with the agent it originated from (or `None`
/// for global events like `ExportComplete`). When the user switches projects
/// mid-stream, the previous task's in-flight events would otherwise pollute
/// the newly-selected project's chat — `dispatch_stream_event` drops events
/// whose tag doesn't match `state.selected_agent_id()`.
struct TaggedEvent {
    agent_id: Option<String>,
    event: ChatStreamEvent,
}

impl TaggedEvent {
    fn for_agent(agent_id: &str, event: ChatStreamEvent) -> Self {
        Self {
            agent_id: Some(agent_id.to_string()),
            event,
        }
    }

    fn untagged(event: ChatStreamEvent) -> Self {
        Self {
            agent_id: None,
            event,
        }
    }
}

enum WorkflowEvent {
    DefinitionsLoaded(Vec<WorkflowDefinitionItem>),
    DefinitionsError(String),
    DefDetailAndRuns {
        def_id: String,
        definition: Option<WorkflowDefinitionDetail>,
        runs: Option<Vec<WorkflowRunItem>>,
        error: Option<String>,
    },
    RunDetail {
        run_id: String,
        detail: Option<WorkflowRunDetail>,
        error: Option<String>,
        is_poll: bool,
    },
    TriggerComplete(Box<WorkflowTriggerOutcome>),
}

struct WorkflowTriggerOutcome {
    filtered: bool,
    run_id: Option<String>,
    run_detail: Option<WorkflowRunDetail>,
    updated_runs: Option<Vec<WorkflowRunItem>>,
    error: Option<String>,
}

fn spawn_chat_stream(
    base_url: String,
    token: String,
    agent_id: String,
    prompt: String,
    tx: mpsc::UnboundedSender<TaggedEvent>,
) {
    tokio::spawn(async move {
        let stream_id = agent_id.clone();
        let send_id = agent_id.clone();
        let result = crate::graphql::subscribe_agent_message(
            &base_url,
            &token,
            &agent_id,
            &prompt,
            |event| {
                let evt = parse_gql_event(&event);
                if let Some(evt) = evt {
                    return tx.send(TaggedEvent::for_agent(&send_id, evt)).is_ok();
                }
                true
            },
        )
        .await;

        if let Err(e) = result {
            let _ = tx.send(TaggedEvent::for_agent(
                &stream_id,
                ChatStreamEvent::Error(e.to_string()),
            ));
        }
        let _ = tx.send(TaggedEvent::for_agent(&stream_id, ChatStreamEvent::Done));
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

/// Best-effort: silently degrades to `None` so a missing/disabled bootstrap
/// mutation doesn't block TUI launch.
async fn bootstrap_personal_project(client: &GraphqlClient) -> Option<String> {
    let resp: Result<BootstrapResponse> = client
        .query(BOOTSTRAP_MUTATION, serde_json::json!({}))
        .await;
    match resp {
        Ok(r) => Some(r.bootstrap_personal_project.project.id),
        Err(_) => None,
    }
}

async fn create_project(client: &GraphqlClient, name: &str, description: &str) -> Result<String> {
    let mut input = serde_json::json!({ "name": name });
    if !description.is_empty() {
        input["description"] = serde_json::json!(description);
    }
    let resp: ProjectCreateResponse = client
        .query(
            PROJECT_CREATE_MUTATION,
            serde_json::json!({ "input": input }),
        )
        .await?;
    Ok(resp.project_create.project.name)
}

fn agent_node_to_item(a: AgentNode) -> AgentItem {
    AgentItem {
        id: a.id,
        name: a.name,
        role: a.role,
        model: a.session.model,
        sandbox: a.attached_sandbox.map(|s| SandboxInfo {
            name: s.name,
            mode: s.mode,
        }),
    }
}

fn workflow_trigger_node_to_info(t: WorkflowTriggerNode) -> WorkflowTriggerInfo {
    WorkflowTriggerInfo {
        kind: t.kind,
        provider: t.provider,
        cron_schedule: t.cron_schedule,
        cron_timezone: t.cron_timezone,
        next_run_at: t.next_run_at,
        condition: t.condition,
        webhook_url: t.webhook_url,
    }
}

fn workflow_step_node_to_item(s: WorkflowStepNode) -> WorkflowStepItem {
    WorkflowStepItem {
        name: s.name,
        step_type: s.step_type,
        skill: s.skill,
        tool: s.tool,
        sandbox: s.sandbox,
        sandbox_mode: s.sandbox_mode,
        condition: s.condition,
    }
}

fn workflow_sandbox_node_to_item(s: WorkflowSandboxNode) -> WorkflowSandboxItem {
    WorkflowSandboxItem {
        name: s.name,
        kind: s.kind,
        branch: s.branch,
        repo_url: s.repo_url,
    }
}

fn workflow_step_result_node_to_item(r: WorkflowStepResultNode) -> WorkflowStepResultItem {
    WorkflowStepResultItem {
        name: r.name,
        state: r.state,
        output: r.output,
        error: r.error,
        skipped: r.skipped,
        completed_at: r.completed_at,
    }
}

fn workflow_run_node_to_item(r: WorkflowRunNode) -> WorkflowRunItem {
    WorkflowRunItem {
        id: r.id,
        state: r.state,
        started_at: r.started_at,
        completed_at: r.completed_at,
        step_results: r
            .step_results
            .into_iter()
            .map(workflow_step_result_node_to_item)
            .collect(),
    }
}

fn workflow_run_node_to_detail(r: WorkflowRunNode) -> WorkflowRunDetail {
    WorkflowRunDetail {
        id: r.id,
        definition_id: r.definition_id,
        project_id: r.project_id,
        state: r.state,
        started_at: r.started_at,
        completed_at: r.completed_at,
        trigger_context: r.trigger_context,
        steps_snapshot: r
            .steps_snapshot
            .into_iter()
            .map(workflow_step_node_to_item)
            .collect(),
        step_results: r
            .step_results
            .into_iter()
            .map(workflow_step_result_node_to_item)
            .collect(),
        agents: r.agents.into_iter().map(agent_node_to_item).collect(),
    }
}

fn workflow_definition_node_to_item(d: WorkflowDefinitionNode) -> WorkflowDefinitionItem {
    WorkflowDefinitionItem {
        id: d.id,
        name: d.name,
        description: d.description,
        trigger: workflow_trigger_node_to_info(d.trigger),
        steps: d
            .steps
            .into_iter()
            .map(workflow_step_node_to_item)
            .collect(),
        recent_runs: d
            .recent_runs
            .into_iter()
            .map(workflow_run_node_to_item)
            .collect(),
    }
}

fn workflow_definition_node_to_detail(d: WorkflowDefinitionNode) -> WorkflowDefinitionDetail {
    WorkflowDefinitionDetail {
        id: d.id,
        name: d.name,
        description: d.description,
        yaml: d.yaml,
        trigger: workflow_trigger_node_to_info(d.trigger),
        steps: d
            .steps
            .into_iter()
            .map(workflow_step_node_to_item)
            .collect(),
        sandboxes: d
            .sandboxes
            .into_iter()
            .map(workflow_sandbox_node_to_item)
            .collect(),
        recent_runs: d
            .recent_runs
            .into_iter()
            .map(workflow_run_node_to_item)
            .collect(),
    }
}

async fn fetch_projects(client: &GraphqlClient) -> Result<Vec<ProjectItem>> {
    let resp: ProjectsResponse = client.query(PROJECTS_QUERY, serde_json::json!({})).await?;

    let items = resp
        .projects
        .edges
        .into_iter()
        .map(|edge| {
            let node = edge.node;
            ProjectItem {
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

async fn fetch_workflow_definitions(
    client: &GraphqlClient,
    project_id: &str,
) -> Result<Vec<WorkflowDefinitionItem>> {
    let resp: WorkflowDefinitionsResponse = client
        .query(
            WORKFLOW_DEFINITIONS_QUERY,
            serde_json::json!({ "projectId": project_id }),
        )
        .await?;

    Ok(resp
        .workflow_definitions
        .into_iter()
        .map(workflow_definition_node_to_item)
        .collect())
}

async fn fetch_workflow_definition(
    client: &GraphqlClient,
    definition_id: &str,
) -> Result<WorkflowDefinitionDetail> {
    let resp: WorkflowDefinitionResponse = client
        .query(
            WORKFLOW_DEFINITION_QUERY,
            serde_json::json!({ "id": definition_id }),
        )
        .await?;

    resp.workflow_definition
        .map(workflow_definition_node_to_detail)
        .ok_or_else(|| anyhow::anyhow!("workflow definition not found"))
}

async fn fetch_workflow_runs(
    client: &GraphqlClient,
    definition_id: &str,
) -> Result<Vec<WorkflowRunItem>> {
    let resp: WorkflowRunsResponse = client
        .query(
            WORKFLOW_RUNS_QUERY,
            serde_json::json!({ "definitionId": definition_id, "first": 50 }),
        )
        .await?;

    Ok(resp
        .workflow_runs
        .into_iter()
        .map(workflow_run_node_to_item)
        .collect())
}

async fn fetch_workflow_run(client: &GraphqlClient, run_id: &str) -> Result<WorkflowRunDetail> {
    let resp: WorkflowRunResponse = client
        .query(WORKFLOW_RUN_QUERY, serde_json::json!({ "id": run_id }))
        .await?;

    resp.workflow_run
        .map(workflow_run_node_to_detail)
        .ok_or_else(|| anyhow::anyhow!("workflow run not found"))
}

async fn trigger_workflow(
    client: &GraphqlClient,
    definition_id: &str,
    payload: serde_json::Value,
) -> Result<WorkflowTriggerPayloadNode> {
    let resp: WorkflowTriggerResponse = client
        .query(
            WORKFLOW_TRIGGER_MUTATION,
            serde_json::json!({
                "input": {
                    "definitionId": definition_id,
                    "payload": payload,
                }
            }),
        )
        .await?;
    Ok(resp.workflow_trigger)
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
    tx: mpsc::UnboundedSender<TaggedEvent>,
) {
    tokio::spawn(async move {
        let client = GraphqlClient::new(&base_url, &token);
        match fetch_chat_history(&client, &agent_id).await {
            Ok(messages) => {
                let _ = tx.send(TaggedEvent::for_agent(
                    &agent_id,
                    ChatStreamEvent::HistoryLoaded(messages),
                ));
            }
            Err(e) => {
                let _ = tx.send(TaggedEvent::for_agent(
                    &agent_id,
                    ChatStreamEvent::Error(format!("Failed to load history: {e}")),
                ));
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

/// B=Base, T=Tools, H=beHavioral (avoids B clash), R=Role, N=Notes,
/// S=Skills, K=Knowledgebase (Spaces — avoids S clash with Skills).
fn system_kind_letter(kind: &str) -> char {
    match kind {
        "BASE" => 'B',
        "TOOLS" => 'T',
        "BEHAVIORAL" => 'H',
        "ROLE" => 'R',
        "NOTES" => 'N',
        "SKILLS" => 'S',
        "SPACES" => 'K',
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
    tx: mpsc::UnboundedSender<TaggedEvent>,
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
                    let _ = tx.send(TaggedEvent::untagged(ChatStreamEvent::ExportComplete(
                        format!("Exported to {path}"),
                    )));
                }
                Err(e) => {
                    let _ = tx.send(TaggedEvent::for_agent(
                        &agent_id,
                        ChatStreamEvent::Error(format!("Failed to write {path}: {e}")),
                    ));
                }
            },
            Err(e) => {
                let _ = tx.send(TaggedEvent::for_agent(
                    &agent_id,
                    ChatStreamEvent::Error(format!("Export failed: {e}")),
                ));
            }
        }
    });
}

fn spawn_threads_fetch(
    base_url: String,
    token: String,
    agent_id: String,
    tx: mpsc::UnboundedSender<TaggedEvent>,
) {
    tokio::spawn(async move {
        let client = GraphqlClient::new(&base_url, &token);
        match fetch_threads(&client, &agent_id).await {
            Ok(grid) => {
                let _ = tx.send(TaggedEvent::for_agent(
                    &agent_id,
                    ChatStreamEvent::ThreadsLoaded(Box::new(grid)),
                ));
            }
            Err(e) => {
                let _ = tx.send(TaggedEvent::for_agent(
                    &agent_id,
                    ChatStreamEvent::Error(format!("Failed to load threads: {e}")),
                ));
            }
        }
    });
}

fn spawn_workflow_definitions_fetch(
    base_url: String,
    token: String,
    project_id: String,
    tx: mpsc::UnboundedSender<WorkflowEvent>,
) {
    tokio::spawn(async move {
        let client = GraphqlClient::new(&base_url, &token);
        match fetch_workflow_definitions(&client, &project_id).await {
            Ok(defs) => {
                let _ = tx.send(WorkflowEvent::DefinitionsLoaded(defs));
            }
            Err(e) => {
                let _ = tx.send(WorkflowEvent::DefinitionsError(format!(
                    "Failed to load workflows: {e}"
                )));
            }
        }
    });
}

fn spawn_workflow_def_detail_fetch(
    base_url: String,
    token: String,
    def_id: String,
    tx: mpsc::UnboundedSender<WorkflowEvent>,
) {
    tokio::spawn(async move {
        let client = GraphqlClient::new(&base_url, &token);
        let (def_result, runs_result) = tokio::join!(
            fetch_workflow_definition(&client, &def_id),
            fetch_workflow_runs(&client, &def_id),
        );
        let mut error = None;
        let definition = match def_result {
            Ok(d) => Some(d),
            Err(e) => {
                error = Some(format!("Failed to load definition: {e}"));
                None
            }
        };
        let runs = match runs_result {
            Ok(r) => Some(r),
            Err(e) => {
                error = Some(format!("Failed to load runs: {e}"));
                None
            }
        };
        let _ = tx.send(WorkflowEvent::DefDetailAndRuns {
            def_id,
            definition,
            runs,
            error,
        });
    });
}

fn spawn_workflow_run_fetch(
    base_url: String,
    token: String,
    run_id: String,
    is_poll: bool,
    tx: mpsc::UnboundedSender<WorkflowEvent>,
) {
    tokio::spawn(async move {
        let client = GraphqlClient::new(&base_url, &token);
        let (detail, error) = match fetch_workflow_run(&client, &run_id).await {
            Ok(d) => (Some(d), None),
            Err(e) => (None, Some(format!("Failed to load run: {e}"))),
        };
        let _ = tx.send(WorkflowEvent::RunDetail {
            run_id,
            detail,
            error,
            is_poll,
        });
    });
}

fn spawn_workflow_trigger(
    base_url: String,
    token: String,
    definition_id: String,
    payload: serde_json::Value,
    tx: mpsc::UnboundedSender<WorkflowEvent>,
) {
    tokio::spawn(async move {
        let client = GraphqlClient::new(&base_url, &token);
        match trigger_workflow(&client, &definition_id, payload).await {
            Ok(result) if result.filtered => {
                let _ = tx.send(WorkflowEvent::TriggerComplete(Box::new(
                    WorkflowTriggerOutcome {
                        filtered: true,
                        run_id: None,
                        run_detail: None,
                        updated_runs: None,
                        error: None,
                    },
                )));
            }
            Ok(result) => {
                if let Some(run) = result.run {
                    let run_id = run.id.clone();
                    let run_detail = match fetch_workflow_run(&client, &run_id).await {
                        Ok(detail) => detail,
                        Err(_) => workflow_run_node_to_detail(run),
                    };
                    let updated_runs = fetch_workflow_runs(&client, &definition_id).await.ok();
                    let _ = tx.send(WorkflowEvent::TriggerComplete(Box::new(
                        WorkflowTriggerOutcome {
                            filtered: false,
                            run_id: Some(run_id),
                            run_detail: Some(run_detail),
                            updated_runs,
                            error: None,
                        },
                    )));
                } else {
                    let _ = tx.send(WorkflowEvent::TriggerComplete(Box::new(
                        WorkflowTriggerOutcome {
                            filtered: false,
                            run_id: None,
                            run_detail: None,
                            updated_runs: None,
                            error: None,
                        },
                    )));
                }
            }
            Err(e) => {
                let _ = tx.send(WorkflowEvent::TriggerComplete(Box::new(
                    WorkflowTriggerOutcome {
                        filtered: false,
                        run_id: None,
                        run_detail: None,
                        updated_runs: None,
                        error: Some(format!("Trigger failed: {e}")),
                    },
                )));
            }
        }
    });
}

fn handle_workflow_event(
    state: &mut ScreenState,
    event: WorkflowEvent,
    fetched_def_id: &mut Option<String>,
    fetched_run_id: &mut Option<String>,
    poll_in_flight: &mut bool,
) {
    match event {
        WorkflowEvent::DefinitionsLoaded(defs) => {
            state.replace_workflow_definitions(defs);
        }
        WorkflowEvent::DefinitionsError(e) => {
            state.set_workflow_error(e);
        }
        WorkflowEvent::DefDetailAndRuns {
            def_id,
            definition,
            runs,
            error,
        } => {
            if fetched_def_id.as_deref() != Some(&def_id) {
                return;
            }
            if let Some(detail) = definition {
                state.replace_workflow_definition_detail(detail);
            }
            if let Some(runs) = runs {
                state.replace_workflow_runs(runs);
            }
            if let Some(e) = error {
                state.set_workflow_error(e);
            }
        }
        WorkflowEvent::RunDetail {
            run_id,
            detail,
            error,
            is_poll,
        } => {
            if is_poll {
                *poll_in_flight = false;
            }
            if fetched_run_id.as_deref() != Some(&run_id) {
                return;
            }
            if let Some(detail) = detail {
                state.replace_workflow_run_detail(detail);
            }
            if let Some(e) = error {
                state.set_workflow_error(e);
            }
        }
        WorkflowEvent::TriggerComplete(outcome) => {
            if let Some(error) = outcome.error {
                state.set_workflow_error(error);
                return;
            }
            if outcome.filtered {
                state.status_message =
                    Some("Trigger condition filtered; no run created".to_string());
                return;
            }
            if let Some(ref run_id) = outcome.run_id {
                state.status_message = Some(format!("Workflow run {run_id} created"));
                state.workflows.run_cursor = 0;
                state.workflows.focus = crate::tui::state::MillerFocus::Runs;
                if let Some(detail) = outcome.run_detail {
                    state.replace_workflow_run_detail(detail);
                }
                if let Some(runs) = outcome.updated_runs {
                    state.replace_workflow_runs(runs);
                }
                *fetched_run_id = outcome.run_id;
            } else {
                state.status_message = Some("Workflow trigger returned no run".to_string());
            }
        }
    }
}

fn dispatch_stream_event(state: &mut ScreenState, tagged: TaggedEvent) {
    // Drop tagged events whose originating agent no longer matches the
    // user's selection — e.g. in-flight deltas from a chat stream
    // kicked off in a project the user just switched away from.
    if let Some(agent_id) = tagged.agent_id.as_deref() {
        if state.selected_agent_id().as_deref() != Some(agent_id) {
            return;
        }
    }
    match tagged.event {
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
        ChatStreamEvent::HistoryLoaded(messages) => {
            state.chat_view.assistant.load_history(messages);
            state.chat_view.reset_scroll();
        }
        ChatStreamEvent::ThreadsLoaded(grid) => {
            state.status_message = None;
            state.thread_view = Some(*grid);
            state.focus = Focus::Threads;
        }
        ChatStreamEvent::ExportComplete(msg) => {
            state.status_message = Some(msg);
        }
    }
}

/// Delegates to login flow when credentials are missing/stale, or when the
/// caller passed `--server <url>` that disagrees with the saved config.
async fn ensure_authenticated(server: Option<String>) -> Result<(Config, GraphqlClient, String)> {
    if let Ok(config) = Config::load() {
        let mismatch = server
            .as_deref()
            .is_some_and(|s| s.trim_end_matches('/') != config.server_url.trim_end_matches('/'));

        if mismatch {
            println!(
                "--server {} differs from saved {} — re-authenticating…",
                server.as_deref().unwrap_or(""),
                config.server_url,
            );
            println!();
        } else {
            let client = GraphqlClient::new(&config.server_url, &config.auth_token);
            if let Ok(user_name) = fetch_user_name(&client).await {
                return Ok((config, client, user_name));
            }
            println!("Session expired — starting login flow…");
            println!();
        }
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
    let bootstrap_id = bootstrap_personal_project(&client).await;
    let projects = fetch_projects(&client).await?;

    let landing = config
        .last_project_id
        .as_deref()
        .filter(|id| projects.iter().any(|p| p.id.as_str() == *id))
        .map(|s| s.to_string())
        .or(bootstrap_id);

    let mut state = ScreenState::new(
        projects,
        config.server_url.clone(),
        user_name,
        landing.as_deref(),
    );
    if let Some(id) = state.selected_project().map(|p| p.id.clone()) {
        let _ = Config::save_last_project(&id);
    }

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
    let (stream_tx, mut stream_rx) = mpsc::unbounded_channel::<TaggedEvent>();
    let (workflow_tx, mut workflow_rx) = mpsc::unbounded_channel::<WorkflowEvent>();
    let mut event_stream = EventStream::new();
    let mut persisted_project_id: Option<String> = state.selected_project().map(|p| p.id.clone());
    let mut last_workflow_poll = Instant::now();
    let mut workflow_poll_in_flight = false;
    let mut fetched_def_id: Option<String> = None;
    let mut fetched_run_id: Option<String> = None;
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
                                if let Ok(projects) = fetch_projects(client).await {
                                    state.replace_projects(projects);
                                }
                            }
                            handlers::Action::CreateProject { name, description } => {
                                match create_project(client, &name, &description).await {
                                    Ok(ws_name) => {
                                        state.exit_create_mode();
                                        if let Ok(projects) = fetch_projects(client).await {
                                            state.replace_projects(projects);
                                        }
                                        state.status_message =
                                            Some(format!("Created project: {ws_name}"));
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
                            handlers::Action::OpenWorkflows => {
                                state.enter_workflows();
                                state.workflows.loading = true;
                                fetched_def_id = None;
                                fetched_run_id = None;
                                if let Some(project_id) = state.selected_project().map(|p| p.id.clone()) {
                                    spawn_workflow_definitions_fetch(
                                        config.server_url.clone(),
                                        config.auth_token.clone(),
                                        project_id,
                                        workflow_tx.clone(),
                                    );
                                } else {
                                    state.set_workflow_error("No project selected".to_string());
                                }
                            }
                            handlers::Action::RefreshWorkflows => {
                                state.workflows.loading = true;
                                fetched_def_id = None;
                                fetched_run_id = None;
                                if let Some(project_id) = state.selected_project().map(|p| p.id.clone()) {
                                    spawn_workflow_definitions_fetch(
                                        config.server_url.clone(),
                                        config.auth_token.clone(),
                                        project_id,
                                        workflow_tx.clone(),
                                    );
                                } else {
                                    state.workflows.loading = false;
                                }
                            }
                            handlers::Action::TriggerWorkflow { definition_id, payload } => {
                                state.status_message = Some("Triggering workflow...".to_string());
                                spawn_workflow_trigger(
                                    config.server_url.clone(),
                                    config.auth_token.clone(),
                                    definition_id,
                                    payload,
                                    workflow_tx.clone(),
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
            event = workflow_rx.recv() => {
                if let Some(evt) = event {
                    handle_workflow_event(
                        state, evt,
                        &mut fetched_def_id, &mut fetched_run_id,
                        &mut workflow_poll_in_flight,
                    );
                }
            }
            _ = tokio::time::sleep(timeout) => {}
        }

        let current_project_id = state.selected_project().map(|p| p.id.clone());
        if current_project_id != persisted_project_id {
            if let Some(id) = current_project_id.as_deref() {
                let _ = Config::save_last_project(id);
            }
            persisted_project_id = current_project_id;
        }

        if state.focus == Focus::Workflows {
            let cur_def = state.selected_workflow_definition_id();
            if cur_def != fetched_def_id {
                fetched_def_id = cur_def.clone();
                state.workflows.error = None;
                state.workflows.runs.clear();
                if let Some(def_id) = &cur_def {
                    spawn_workflow_def_detail_fetch(
                        config.server_url.clone(),
                        config.auth_token.clone(),
                        def_id.clone(),
                        workflow_tx.clone(),
                    );
                } else {
                    state.workflows.selected_definition = None;
                }
                state.workflows.selected_run = None;
                fetched_run_id = None;
            }

            let cur_run = state.selected_workflow_run_id();
            if cur_run != fetched_run_id {
                fetched_run_id = cur_run.clone();
                state.workflows.error = None;
                if let Some(run_id) = &cur_run {
                    spawn_workflow_run_fetch(
                        config.server_url.clone(),
                        config.auth_token.clone(),
                        run_id.clone(),
                        false,
                        workflow_tx.clone(),
                    );
                } else {
                    state.workflows.selected_run = None;
                }
            }
        }

        if last_workflow_poll.elapsed() >= Duration::from_secs(1) {
            last_workflow_poll = Instant::now();
            if !workflow_poll_in_flight {
                if let Some(run_id) = state.workflow_poll_run_id() {
                    workflow_poll_in_flight = true;
                    spawn_workflow_run_fetch(
                        config.server_url.clone(),
                        config.auth_token.clone(),
                        run_id,
                        true,
                        workflow_tx.clone(),
                    );
                }
            }
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
