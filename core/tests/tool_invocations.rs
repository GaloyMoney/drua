//! End-to-end smoke for the universal-pipeline persistence + fetch loop.
//!
//! - Persist a synthetic invocation via [`ToolInvocations`].
//! - Re-read it via the same service through every [`FetchQuery`] mode.
//! - Confirm the FK + indexes resolve against a live PG.

use std::sync::Arc;

use chrono::Utc;
use rmcp::model::{CallToolResult, Content, JsonObject};

use drua_core::auth::AuthSubject;
use drua_core::primitives::{AgentId, ProjectId, UserId, WorkflowDefinitionId, WorkflowRunId};
use drua_core::toolset::tool_invocations::{
    FetchQuery, NewToolInvocation, ToolInvocationOwner, ToolInvocations,
};
use drua_core::toolset::{ToolSets, ToolSetsConfig, ToolSetsError, TopLevelTool};

const PG_CON: &str = "postgres://user:password@localhost:5432/drua";

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| PG_CON.to_string());
    sqlx::PgPool::connect(&url).await.expect("connect to pg")
}

async fn insert_project(pool: &sqlx::PgPool) -> ProjectId {
    let id = ProjectId::new();
    let name = format!("ti-test-project-{}", uuid::Uuid::from(id));
    sqlx::query("INSERT INTO projects (id, name, created_at) VALUES ($1, $2, NOW())")
        .bind(id)
        .bind(&name)
        .execute(pool)
        .await
        .expect("insert project");
    id
}

/// Inserts a bare agents row so the `tool_invocations.agent_id` FK resolves
/// without going through the full `Agents` service. Mirrors `insert_project`.
/// `agents` is event-sourced (notes/agent pattern) — non-key columns live in
/// the events JSONB, so the projected row just carries the FKs.
async fn insert_agent(pool: &sqlx::PgPool, project_id: ProjectId) -> AgentId {
    let id = AgentId::new();
    sqlx::query(
        "INSERT INTO agents (id, project_id, created_at, deleted)
         VALUES ($1, $2, NOW(), FALSE)",
    )
    .bind(id)
    .bind(project_id)
    .execute(pool)
    .await
    .expect("insert agent");
    id
}

fn sample_raw() -> String {
    let mut lines = Vec::new();
    for i in 0..200 {
        if i == 47 {
            lines.push("error: this is the line we want to find".to_string());
        } else {
            lines.push(format!("line-{i:04}"));
        }
    }
    lines.join("\n")
}

/// Inserts a bare users row so the `tool_invocations.user_id` FK
/// resolves. `github_id` is unique-per-row in production — this fixture
/// just needs a value that doesn't collide with parallel test runs, so
/// we reuse the row id.
async fn insert_user(pool: &sqlx::PgPool) -> UserId {
    let id = UserId::new();
    let github_id = format!("ti-test-{}", uuid::Uuid::from(id));
    sqlx::query("INSERT INTO users (id, github_id, created_at) VALUES ($1, $2, NOW())")
        .bind(id)
        .bind(&github_id)
        .execute(pool)
        .await
        .expect("insert user");
    id
}

fn build_new(owner: ToolInvocationOwner, raw: &str) -> NewToolInvocation {
    NewToolInvocation {
        owner,
        tool_name: "bash".to_string(),
        args: serde_json::json!({"command": "echo demo"}),
        args_hash: vec![1, 2, 3, 4],
        classifier: "generic::v1".to_string(),
        summary: serde_json::json!({"kind": "generic", "total_bytes": raw.len()}),
        raw_text: raw.to_string(),
        raw_size_bytes: raw.len() as i64,
        original_structured: None,
        exit_code: None,
        duration_ms: 42,
        started_at: Utc::now(),
    }
}

#[tokio::test]
async fn persist_round_trip_and_fetch_modes() {
    let pool = pool().await;
    let invocations = ToolInvocations::new(&pool);

    let project_id = insert_project(&pool).await;
    let agent_id = insert_agent(&pool, project_id).await;

    let raw = sample_raw();
    let new = build_new(ToolInvocationOwner::Agent { agent_id }, &raw);
    let persisted = invocations.persist(new).await.expect("persist");

    assert_eq!(persisted.owner, ToolInvocationOwner::Agent { agent_id });
    assert_eq!(persisted.classifier, "generic::v1");
    assert_eq!(persisted.raw_size_bytes, raw.len() as i64);
    assert_eq!(persisted.raw_text, raw);

    // Tail: last 3 lines of the synthetic body.
    let tail = invocations
        .fetch(persisted.id, FetchQuery::Tail { lines: 3 })
        .await
        .expect("tail fetch");
    assert_eq!(tail.content, "line-0197\nline-0198\nline-0199");
    assert!(tail.elision.is_none());
    assert_eq!(tail.total_bytes, raw.len() as u64);

    // Head: first 2 lines.
    let head = invocations
        .fetch(persisted.id, FetchQuery::Head { lines: 2 })
        .await
        .expect("head fetch");
    assert_eq!(head.content, "line-0000\nline-0001");

    // Range: byte slice covering the first line.
    let range = invocations
        .fetch(persisted.id, FetchQuery::Range { offset: 0, len: 9 })
        .await
        .expect("range fetch");
    assert_eq!(range.content, "line-0000");

    // Grep: lands on the seeded error line.
    let grep = invocations
        .fetch(
            persisted.id,
            FetchQuery::Grep {
                pattern: "^error".to_string(),
                case_insensitive: false,
                after_context: None,
                before_context: None,
                context: None,
                line_numbers: false,
                invert_match: false,
                head_limit: None,
            },
        )
        .await
        .expect("grep fetch");
    assert_eq!(grep.content, "error: this is the line we want to find");
}

/// Stub tool whose only behaviour is returning a fixed-size synthetic
/// payload. The `bypass` flag toggles the universal-pipeline opt-out so
/// one test exercises both branches without pulling in real upstream
/// tools.
struct StubTool {
    name: &'static str,
    body: String,
    bypass: bool,
}

#[async_trait::async_trait]
impl TopLevelTool for StubTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "stub for dispatcher pipeline tests"
    }
    fn input_schema(&self) -> &serde_json::Value {
        static EMPTY: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        EMPTY.get_or_init(|| {
            serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false})
        })
    }
    fn bypass_universal_pipeline(&self) -> bool {
        self.bypass
    }
    async fn call(
        &self,
        _subject: &AuthSubject,
        _arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        Ok(CallToolResult::success(vec![Content::text(
            self.body.clone(),
        )]))
    }
}

/// Cursor Bugbot review on PR #309: `tool_output_fetch`'s response (up to
/// 32 KB by design) was getting re-classified as `Generic` and wrapped in
/// a fresh envelope, defeating the recovery escape hatch. Regression
/// guard: a tool that opts out via `bypass_universal_pipeline()` must
/// reach the model unchanged even when its body would otherwise trigger
/// `GenericFallback`.
#[tokio::test]
async fn bypass_universal_pipeline_skips_classifier_and_persistence() {
    let pool = pool().await;
    let project_id = insert_project(&pool).await;
    let agent_id = insert_agent(&pool, project_id).await;

    let tool_invocations = Arc::new(ToolInvocations::new(&pool));
    let toolsets = ToolSets::init(
        ToolSetsConfig::default(),
        None,
        None,
        Some(Arc::clone(&tool_invocations)),
    )
    .await
    .expect("ToolSets::init");

    // Both bodies are way over the 4 KB GenericFallback threshold; only
    // the bypass flag should distinguish them.
    let body = "x".repeat(50_000);
    toolsets.register_top_level(StubTool {
        name: "stub-bypass",
        body: body.clone(),
        bypass: true,
    });
    toolsets.register_top_level(StubTool {
        name: "stub-no-bypass",
        body: body.clone(),
        bypass: false,
    });

    // Subject carries the agent_id so `subject.acting_agent_id()` lands
    // in the persistence branch of the dispatcher; an Anonymous subject
    // would short-circuit to passthrough regardless of the bypass flag.
    let agent_subject = AuthSubject::Agent(project_id, agent_id, Vec::new());

    let bypass_result = toolsets
        .call_top_level_tool(&agent_subject, "stub-bypass", None)
        .await
        .expect("bypass tool dispatch");
    // Raw passthrough: text is the original body, structured_content
    // unset, no envelope.
    let bypass_text: String = bypass_result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    assert_eq!(
        bypass_text, body,
        "bypass tool's body must reach the model verbatim"
    );
    assert!(
        bypass_result.structured_content.is_none(),
        "bypass tool must not be re-wrapped in an envelope; got: {:?}",
        bypass_result.structured_content,
    );

    let wrapped_result = toolsets
        .call_top_level_tool(&agent_subject, "stub-no-bypass", None)
        .await
        .expect("non-bypass tool dispatch");
    // Confirm the test setup is real: the same body, without the
    // bypass flag, DOES get wrapped — proves the bypass branch is
    // load-bearing, not a no-op.
    let envelope = wrapped_result
        .structured_content
        .as_ref()
        .expect("non-bypass body over the threshold must be wrapped");
    let invocation_id = envelope
        .get("invocation_id")
        .and_then(|v| v.as_str())
        .expect("non-bypass envelope must include invocation_id");
    assert!(
        !invocation_id.is_empty(),
        "non-bypass envelope must carry a non-empty invocation_id",
    );
}

#[tokio::test]
async fn find_for_diff_returns_latest_matching_args_hash() {
    let pool = pool().await;
    let invocations = ToolInvocations::new(&pool);
    let project_id = insert_project(&pool).await;
    let agent_id = insert_agent(&pool, project_id).await;
    let agent_owner = ToolInvocationOwner::Agent { agent_id };

    // Two distinct invocations with the same args_hash; expect the most
    // recent to come back.
    invocations
        .persist(build_new(agent_owner, "first"))
        .await
        .expect("persist first");
    let second = invocations
        .persist(build_new(agent_owner, "second"))
        .await
        .expect("persist second");

    let hit = invocations
        .find_for_diff(agent_owner, &[1, 2, 3, 4])
        .await
        .expect("find_for_diff");
    let hit = hit.expect("matching invocation");
    assert_eq!(hit.id, second.id);
    assert_eq!(hit.raw_text, "second");

    // Distinct hash → no hit.
    let miss = invocations
        .find_for_diff(agent_owner, &[9, 9, 9, 9])
        .await
        .expect("find_for_diff non-matching");
    assert!(miss.is_none());
}

/// `AuthSubject::WorkflowExecutor` (introduced in PR #306) dispatches
/// composable tools whose `structured_content` is the workflow step's
/// typed output — wrapping that result in a recovery envelope would
/// replace the structured payload with summary JSON the workflow
/// can't consume. The pipeline must short-circuit to raw passthrough
/// for this subject, exactly the way it does for `Anonymous`. This
/// test guards the contract.
#[tokio::test]
async fn workflow_executor_subject_skips_envelope_to_preserve_structured_output() {
    let pool = pool().await;

    let tool_invocations = Arc::new(ToolInvocations::new(&pool));
    let toolsets = ToolSets::init(
        ToolSetsConfig::default(),
        None,
        None,
        Some(Arc::clone(&tool_invocations)),
    )
    .await
    .expect("ToolSets::init");

    // 50 KB body — well over the 4 KB threshold; the only thing
    // keeping it from getting wrapped is the subject's scope rule.
    let body = "z".repeat(50_000);
    toolsets.register_top_level(StubTool {
        name: "stub-workflow-tool",
        body: body.clone(),
        bypass: false,
    });

    let subject = AuthSubject::workflow_executor(
        ProjectId::new(),
        WorkflowDefinitionId::new(),
        WorkflowRunId::new(),
    );
    let result = toolsets
        .call_top_level_tool(&subject, "stub-workflow-tool", None)
        .await
        .expect("workflow-executor dispatch");

    let text: String = result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    assert_eq!(
        text, body,
        "WorkflowExecutor's tool body must reach the workflow verbatim"
    );
    assert!(
        result.structured_content.is_none(),
        "WorkflowExecutor result must NOT be wrapped in an envelope; \
         structured_content was: {:?}",
        result.structured_content,
    );
}

/// User-rooted dispatch: mcp-gateway external callers run with
/// `AuthSubject::User`, no agent in scope. The universal pipeline
/// must still wrap their oversized results — the recovery handle is
/// the load-bearing part for cli/IDE-driven tool calls too. The
/// previous shape that short-circuited any non-agent subject to raw
/// passthrough is what this test guards against regressing into.
#[tokio::test]
async fn user_subject_gets_pipeline_wrapping_via_mcp_gateway_path() {
    let pool = pool().await;
    let user_id = insert_user(&pool).await;

    let tool_invocations = Arc::new(ToolInvocations::new(&pool));
    let toolsets = ToolSets::init(
        ToolSetsConfig::default(),
        None,
        None,
        Some(Arc::clone(&tool_invocations)),
    )
    .await
    .expect("ToolSets::init");

    let body = "y".repeat(50_000);
    toolsets.register_top_level(StubTool {
        name: "stub-user-call",
        body: body.clone(),
        bypass: false,
    });

    let user_subject = AuthSubject::User(user_id);
    let result = toolsets
        .call_top_level_tool(&user_subject, "stub-user-call", None)
        .await
        .expect("user-scoped dispatch");

    let envelope = result
        .structured_content
        .as_ref()
        .expect("user-scoped body over the threshold must still be wrapped");
    let invocation_id_str = envelope
        .get("invocation_id")
        .and_then(|v| v.as_str())
        .expect("envelope must include invocation_id for user subject");
    let invocation_id = uuid::Uuid::parse_str(invocation_id_str)
        .expect("invocation_id is a uuid")
        .into();

    let persisted = tool_invocations
        .find_by_id(invocation_id)
        .await
        .expect("persisted row");
    assert_eq!(
        persisted.owner,
        ToolInvocationOwner::User { user_id },
        "user-rooted call must persist with the User owner shape",
    );
}

/// User-rooted scope: mcp-gateway external callers (subject =
/// `AuthSubject::User`) get their own diff probe. Agent and user
/// scopes are independent — same `args_hash` under different owners
/// must not collide.
#[tokio::test]
async fn find_for_diff_user_scope_is_isolated_from_agent_scope() {
    let pool = pool().await;
    let invocations = ToolInvocations::new(&pool);
    let project_id = insert_project(&pool).await;
    let agent_id = insert_agent(&pool, project_id).await;
    let user_id = insert_user(&pool).await;

    let agent_owner = ToolInvocationOwner::Agent { agent_id };
    let user_owner = ToolInvocationOwner::User { user_id };

    invocations
        .persist(build_new(agent_owner, "for-agent"))
        .await
        .expect("persist agent-scoped");
    let user_row = invocations
        .persist(build_new(user_owner, "for-user"))
        .await
        .expect("persist user-scoped");

    let user_hit = invocations
        .find_for_diff(user_owner, &[1, 2, 3, 4])
        .await
        .expect("find_for_diff user")
        .expect("user-scoped match");
    assert_eq!(user_hit.id, user_row.id);
    assert_eq!(user_hit.raw_text, "for-user");
    assert_eq!(user_hit.owner, user_owner);
}
