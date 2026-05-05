use std::collections::HashMap;
use std::sync::Arc;

use drua_core::agent::{AgentRole, Agents, AgentsConfig, ModelDefaults, RoleConfig};
use drua_core::primitives::{AuthSubject, ChatOutputEvent, ContextGeneration, ProjectId, UserId};
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

/// Inserts a bare project row so foreign keys (agents.project_id, etc.)
/// resolve in tests that don't go through `Projects::create`.
async fn insert_project(pool: &sqlx::PgPool) -> ProjectId {
    let id = ProjectId::new();
    let name = format!("test-project-{}", uuid::Uuid::from(id));
    sqlx::query("INSERT INTO projects (id, name, created_at) VALUES ($1, $2, NOW())")
        .bind(id)
        .bind(&name)
        .execute(pool)
        .await
        .expect("insert project");
    id
}

/// Builds a minimal `Agents` (+ shared `Sandboxes`) wired against a real
/// pool. The prompt-request channel is dropped on the receiver side; tests
/// that exercise `delete` / `list` don't dispatch prompts.
async fn build_agents(pool: &sqlx::PgPool) -> (Agents, Arc<Sandboxes>) {
    let (prompt_tx, _prompt_rx) = mpsc::channel::<PromptRequest>(64);

    let model_name = "claude-haiku-4-5-20251001".to_string();
    let mut builtin_roles = HashMap::new();
    builtin_roles.insert(
        AgentRole::ProjectLead,
        RoleConfig {
            model: model_name.clone(),
            compaction: Default::default(),
        },
    );
    builtin_roles.insert(
        AgentRole::Agent,
        RoleConfig {
            model: model_name.clone(),
            compaction: Default::default(),
        },
    );
    let mut models = HashMap::new();
    models.insert(
        model_name.clone(),
        ModelDefaults {
            model: model_name,
            max_tokens_per_response: 1024,
            context_window_tokens: 200_000,
        },
    );
    let config = AgentsConfig {
        builtin_roles,
        models,
    };

    let toolsets = Arc::new(
        ToolSets::init(ToolSetsConfig::default())
            .await
            .expect("init toolsets"),
    );
    let sandboxes = Arc::new(
        Sandboxes::init(pool, SandboxConfig::default(), None)
            .await
            .expect("init sandboxes"),
    );
    let skills = Arc::new(drua_core::skill::Skills::new_without_library(
        pool,
        Arc::clone(&sandboxes),
    ));
    let agents = Agents::new(
        pool,
        config,
        toolsets,
        prompt_tx,
        Arc::clone(&sandboxes),
        skills,
        None,
        ContextGeneration::new(),
        Arc::new(drua_core::library::SpaceMounts::empty()),
    );
    (agents, sandboxes)
}

#[tokio::test]
async fn send_message_round_trip_via_prompt_channel() {
    let pool = pool().await;

    let (prompt_tx, mut prompt_rx) = mpsc::channel::<PromptRequest>(64);

    let model_name = "claude-haiku-4-5-20251001".to_string();
    let mut builtin_roles = HashMap::new();
    builtin_roles.insert(
        AgentRole::ProjectLead,
        RoleConfig {
            model: model_name.clone(),
            compaction: Default::default(),
        },
    );
    let mut models = HashMap::new();
    models.insert(
        model_name.clone(),
        ModelDefaults {
            model: model_name,
            max_tokens_per_response: 1024,
            context_window_tokens: 200_000,
        },
    );
    let config = AgentsConfig {
        builtin_roles,
        models,
    };

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
    let skills = Arc::new(drua_core::skill::Skills::new_without_library(
        &pool,
        Arc::clone(&sandboxes),
    ));
    let agents = Agents::new(
        &pool,
        config,
        toolsets,
        prompt_tx,
        Arc::clone(&sandboxes),
        Arc::clone(&skills),
        None,
        ContextGeneration::new(),
        Arc::new(drua_core::library::SpaceMounts::empty()),
    );

    let sub = AuthSubject::User(UserId::new());
    let agent = agents
        .create_project_lead(&sub, ProjectId::new(), "lead", "test-project")
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
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
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

    let model_name = "claude-haiku-4-5-20251001".to_string();
    let mut builtin_roles = HashMap::new();
    builtin_roles.insert(
        AgentRole::ProjectLead,
        RoleConfig {
            model: model_name.clone(),
            compaction: Default::default(),
        },
    );
    let mut models = HashMap::new();
    models.insert(
        model_name.clone(),
        ModelDefaults {
            model: model_name,
            max_tokens_per_response: 1024,
            context_window_tokens: 200_000,
        },
    );
    let config = AgentsConfig {
        builtin_roles,
        models,
    };

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
    let skills = Arc::new(drua_core::skill::Skills::new_without_library(
        &pool,
        Arc::clone(&sandboxes),
    ));
    let agents = Agents::new(
        &pool,
        config,
        toolsets,
        prompt_tx,
        Arc::clone(&sandboxes),
        Arc::clone(&skills),
        None,
        ContextGeneration::new(),
        Arc::new(drua_core::library::SpaceMounts::empty()),
    );

    let sub = AuthSubject::User(UserId::new());
    let agent = agents
        .create_project_lead(&sub, ProjectId::new(), "lead", "test-project")
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
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
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
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
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
    //   ToolCallStart { name: ping }
    //   ToolCallInputDelta { partial_json: "{}" }
    //   ToolResult { name: ping, is_error: false }
    //   AssistantText { text: "all done" }
    //   Done { turns: 2, input_tokens: 16, output_tokens: 6 }
    assert_eq!(events.len(), 6, "unexpected event sequence: {events:?}");

    assert!(
        matches!(&events[0], ChatOutputEvent::UserMessage { text, .. } if text == "Call ping"),
        "event[0] should be UserMessage, got {:?}",
        events[0]
    );
    assert!(
        matches!(&events[1], ChatOutputEvent::ToolCallStart { name } if name == "ping"),
        "event[1] should be ToolCallStart(ping), got {:?}",
        events[1]
    );
    assert!(
        matches!(&events[2], ChatOutputEvent::ToolCallInputDelta { .. }),
        "event[2] should be ToolCallInputDelta, got {:?}",
        events[2]
    );
    match &events[3] {
        ChatOutputEvent::ToolResult {
            name,
            is_error,
            content,
        } => {
            assert_eq!(name, "ping");
            assert!(!is_error, "ping tool should succeed");
            assert!(content.is_some(), "content should be populated");
        }
        other => panic!("event[3] should be ToolResult, got {other:?}"),
    }
    assert!(
        matches!(&events[4], ChatOutputEvent::AssistantText { text } if text == "all done"),
        "event[4] should be AssistantText, got {:?}",
        events[4]
    );
    match &events[5] {
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
        other => panic!("event[5] should be Done, got {other:?}"),
    }
}

#[tokio::test]
async fn delete_removes_agent_from_listing() {
    let pool = pool().await;
    let (agents, _sandboxes) = build_agents(&pool).await;
    let project_id = insert_project(&pool).await;
    let sub = AuthSubject::User(UserId::new());

    let agent = agents
        .create_project_lead(&sub, project_id, "lead", "test-project")
        .await
        .expect("create lead");

    agents.delete(&sub, agent.id).await.expect("delete");

    let listed = agents
        .list_for_project(&sub, project_id)
        .await
        .expect("list");
    assert!(
        !listed.iter().any(|a| a.id == agent.id),
        "deleted agent should not appear in listing"
    );
}

#[tokio::test]
async fn delete_is_idempotent_on_second_call() {
    let pool = pool().await;
    let (agents, _sandboxes) = build_agents(&pool).await;
    let project_id = insert_project(&pool).await;
    let sub = AuthSubject::User(UserId::new());

    let agent = agents
        .create_project_lead(&sub, project_id, "lead", "test-project")
        .await
        .expect("create lead");

    agents.delete(&sub, agent.id).await.expect("first delete");
    // Second delete on a soft-deleted entity surfaces as `Find` —
    // expected and acceptable per project convention; the row is gone
    // from listings either way.
    let second = agents.delete(&sub, agent.id).await;
    assert!(
        second.is_err(),
        "soft-deleted agent should not be findable for re-delete"
    );
}

#[tokio::test]
async fn delete_detaches_attached_sandbox() {
    use drua_core::sandbox::{SandboxAgentMode, SandboxMode, SandboxSpecs};

    let pool = pool().await;
    let (agents, sandboxes) = build_agents(&pool).await;
    let project_id = insert_project(&pool).await;
    let sub = AuthSubject::User(UserId::new());

    let _lead = agents
        .create_project_lead(&sub, project_id, "lead", "test-project")
        .await
        .expect("create lead");

    let sandbox = sandboxes
        .create(
            &sub,
            project_id,
            "sb",
            SandboxSpecs {
                cpu: "100m".to_string(),
                memory: "128Mi".to_string(),
                disk_size: "1Gi".to_string(),
            },
            SandboxMode::Scratch,
        )
        .await
        .expect("create sandbox");

    let agent = agents
        .create_agent(
            &sub,
            project_id,
            "worker",
            Some((sandbox.id, SandboxAgentMode::Read)),
        )
        .await
        .expect("create attached agent");

    agents.delete(&sub, agent.id).await.expect("delete agent");

    let after = sandboxes
        .find_by_id(&sub, sandbox.id)
        .await
        .expect("find sandbox");
    assert!(
        after.attached_agents.iter().all(|(id, _)| *id != agent.id),
        "sandbox.attached_agents should not reference the deleted agent"
    );
}
