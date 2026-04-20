use std::collections::HashMap;
use std::sync::Arc;

use drua_core::agent::{AgentRole, Agents, AgentsConfig, RoleConfig};
use drua_core::primitives::{AuthSubject, ChatOutputEvent, UserId, WorkspaceId};
use drua_core::sandbox::{SandboxConfig, Sandboxes};
use drua_core::toolset::{ToolSets, ToolSetsConfig, ToolSetsError, TopLevelTool};
use llm::prompt::AssistantBlock;
use llm::response::StopReason;
use llm::{PromptRequest, PromptResponse, PromptResult, Usage};
use rmcp::model::{CallToolResult, Content, JsonObject};
use tokio::sync::mpsc;

const PG_CON: &str = "postgres://user:password@localhost:5432/drua";

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| PG_CON.to_string());
    sqlx::PgPool::connect(&url).await.expect("connect to pg")
}

#[tokio::test]
async fn send_message_round_trip_via_prompt_channel() {
    let pool = pool().await;

    let (prompt_tx, mut prompt_rx) = mpsc::channel::<PromptRequest>(64);

    let mut builtin_roles = HashMap::new();
    builtin_roles.insert(
        AgentRole::WorkspaceLead,
        RoleConfig {
            model: "claude-haiku-4-5-20251001".to_string(),
            max_tokens: 1024,
            reset_time_delta_seconds: None,
            compaction: Default::default(),
        },
    );
    let config = AgentsConfig { builtin_roles };

    let toolsets = Arc::new(
        ToolSets::init(ToolSetsConfig::default())
            .await
            .expect("init toolsets"),
    );

    let sandboxes = Arc::new(
        Sandboxes::init(&pool, SandboxConfig::default(), None)
            .await
            .expect("init sandboxes"),
    );
    let skills = Arc::new(drua_core::skill::Skills::new(&pool, Arc::clone(&sandboxes)));
    let agents = Agents::new(
        &pool,
        config,
        toolsets,
        prompt_tx,
        Arc::clone(&sandboxes),
        Arc::clone(&skills),
    );

    let sub = AuthSubject::User(UserId::new());
    let agent = agents
        .create_workspace_lead(&sub, WorkspaceId::new(), "lead", "test-workspace")
        .await
        .expect("create agent");

    let mut events_rx = agents
        .send_message(sub, agent.id, "Hello agent".to_string())
        .await
        .expect("send_message");

    // Evaluator side: receive the dispatched prompt request, send one
    // PromptResponse with a single text block + usage metadata, then drop the
    // response channel so the forwarder loop terminates on its own.
    let request = prompt_rx.recv().await.expect("prompt request dispatched");
    request
        .response_channel
        .send(Ok(PromptResult::Complete(PromptResponse {
            content: vec![AssistantBlock::Text {
                text: "Hi user".to_string(),
                cache_control: None,
            }],
            usage: Usage {
                input_tokens: 5,
                output_tokens: 3,
            },
            stop_reason: None,
        })))
        .expect("send response");

    // Drain the ChatOutputEvent channel until the forwarder closes it.
    let mut events = Vec::new();
    while let Some(event) = events_rx.recv().await {
        events.push(event);
    }

    assert_eq!(
        events.len(),
        3,
        "expected user echo + assistant text + done, got {events:?}"
    );

    match &events[0] {
        ChatOutputEvent::UserMessage { text, .. } => assert_eq!(text, "Hello agent"),
        other => panic!("event[0] should be UserMessage, got {other:?}"),
    }
    match &events[1] {
        ChatOutputEvent::AssistantText { text } => assert_eq!(text, "Hi user"),
        other => panic!("event[1] should be AssistantText, got {other:?}"),
    }
    match &events[2] {
        ChatOutputEvent::AssistantDone {
            input_tokens,
            output_tokens,
            ..
        } => {
            assert_eq!(*input_tokens, 5);
            assert_eq!(*output_tokens, 3);
        }
        other => panic!("event[2] should be Done, got {other:?}"),
    }
}

/// A registered top-level tool that always returns "pong". Lets us drive a
/// real tool-call round trip end-to-end.
struct PingTool {
    schema: serde_json::Value,
}

impl PingTool {
    fn new() -> Self {
        Self {
            schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }),
        }
    }
}

#[async_trait::async_trait]
impl TopLevelTool for PingTool {
    fn name(&self) -> &str {
        "ping"
    }
    fn description(&self) -> &str {
        "Returns pong. Test-only tool."
    }
    fn input_schema(&self) -> &serde_json::Value {
        &self.schema
    }
    async fn call(
        &self,
        _subject: &AuthSubject,
        _arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        Ok(CallToolResult::success(vec![Content::text("pong")]))
    }
}

#[tokio::test]
async fn send_message_dispatches_registered_tool_call() {
    let pool = pool().await;

    let (prompt_tx, mut prompt_rx) = mpsc::channel::<PromptRequest>(64);

    let mut builtin_roles = HashMap::new();
    builtin_roles.insert(
        AgentRole::WorkspaceLead,
        RoleConfig {
            model: "claude-haiku-4-5-20251001".to_string(),
            max_tokens: 1024,
            reset_time_delta_seconds: None,
            compaction: Default::default(),
        },
    );
    let config = AgentsConfig { builtin_roles };

    // Build ToolSets, register the test tool, then share via Arc.
    let toolsets = ToolSets::init(ToolSetsConfig::default())
        .await
        .expect("init toolsets");
    toolsets.register_top_level(PingTool::new());
    let toolsets = Arc::new(toolsets);

    let sandboxes = Arc::new(
        Sandboxes::init(&pool, SandboxConfig::default(), None)
            .await
            .expect("init sandboxes"),
    );
    let skills = Arc::new(drua_core::skill::Skills::new(&pool, Arc::clone(&sandboxes)));
    let agents = Agents::new(
        &pool,
        config,
        toolsets,
        prompt_tx,
        Arc::clone(&sandboxes),
        Arc::clone(&skills),
    );

    let sub = AuthSubject::User(UserId::new());
    let agent = agents
        .create_workspace_lead(&sub, WorkspaceId::new(), "lead", "test-workspace")
        .await
        .expect("create agent");

    let mut events_rx = agents
        .send_message(sub, agent.id, "Call ping".to_string())
        .await
        .expect("send_message");

    // First turn: respond with a tool_use block calling `ping`.
    let request = prompt_rx.recv().await.expect("first prompt request");
    assert!(
        request.prompt.tools.iter().any(|t| t.name == "ping"),
        "first prompt's tools should include `ping`, got {:?}",
        request
            .prompt
            .tools
            .iter()
            .map(|t| &t.name)
            .collect::<Vec<_>>(),
    );
    request
        .response_channel
        .send(Ok(PromptResult::Complete(PromptResponse {
            content: vec![AssistantBlock::ToolUse {
                id: "tu_1".to_string(),
                name: "ping".to_string(),
                input: serde_json::json!({}),
                cache_control: None,
            }],
            usage: Usage {
                input_tokens: 7,
                output_tokens: 4,
            },
            stop_reason: Some(StopReason::ToolUse),
        })))
        .expect("send first response");

    // Second turn: the agent should now dispatch a follow-up request that
    // includes the tool result. Reply with a final text block.
    let request = prompt_rx
        .recv()
        .await
        .expect("second prompt request after tool result");
    request
        .response_channel
        .send(Ok(PromptResult::Complete(PromptResponse {
            content: vec![AssistantBlock::Text {
                text: "all done".to_string(),
                cache_control: None,
            }],
            usage: Usage {
                input_tokens: 9,
                output_tokens: 2,
            },
            stop_reason: None,
        })))
        .expect("send second response");

    let mut events = Vec::new();
    while let Some(event) = events_rx.recv().await {
        events.push(event);
    }

    // Expected sequence:
    //   UserMessage echo
    //   ToolCall { name: ping }
    //   ToolResult { name: ping, is_error: false }
    //   AssistantText { text: "all done" }
    //   Done { turns: 2, input_tokens: 16, output_tokens: 6 }
    assert_eq!(events.len(), 5, "unexpected event sequence: {events:?}");

    assert!(
        matches!(&events[0], ChatOutputEvent::UserMessage { text, .. } if text == "Call ping"),
        "event[0] should be UserMessage, got {:?}",
        events[0]
    );
    assert!(
        matches!(&events[1], ChatOutputEvent::ToolCall { name, .. } if name == "ping"),
        "event[1] should be ToolCall(ping), got {:?}",
        events[1]
    );
    match &events[2] {
        ChatOutputEvent::ToolResult { name, is_error } => {
            assert_eq!(name, "ping");
            assert!(!is_error, "ping tool should succeed");
        }
        other => panic!("event[2] should be ToolResult, got {other:?}"),
    }
    assert!(
        matches!(&events[3], ChatOutputEvent::AssistantText { text } if text == "all done"),
        "event[3] should be AssistantText, got {:?}",
        events[3]
    );
    match &events[4] {
        ChatOutputEvent::AssistantDone {
            turns,
            input_tokens,
            output_tokens,
            ..
        } => {
            assert_eq!(*turns, 2, "should count both LLM rounds");
            assert_eq!(*input_tokens, 16);
            assert_eq!(*output_tokens, 6);
        }
        other => panic!("event[4] should be Done, got {other:?}"),
    }
}
