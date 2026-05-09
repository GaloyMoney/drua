//! End-to-end tests for compose's recovery surface.

use std::sync::Arc;

use rmcp::model::{CallToolResult, Content, JsonObject, Tool};
use serde_json::json;

use drua_core::audit::Audit;
use drua_core::auth::AuthSubject;
use drua_core::primitives::{ProjectId, UserId, WorkflowDefinitionId, WorkflowRunId};
use drua_core::toolset::{
    SearchableToolSet, ToolInvocations, ToolSetEntry, ToolSets, ToolSetsConfig, ToolSetsError,
};

const PG_CON: &str = "postgres://user:password@localhost:5432/drua";

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| PG_CON.to_string());
    sqlx::PgPool::connect(&url).await.expect("connect to pg")
}

struct StubSet {
    name: String,
    tools: Vec<ToolSetEntry>,
    payload_bytes: usize,
}

impl StubSet {
    fn new(name: &str, tool_name: &str, payload_bytes: usize) -> Self {
        let mut tool = Tool::default();
        tool.name = tool_name.to_string().into();
        tool.description = Some("stub tool".to_string().into());
        tool.input_schema = Arc::new(JsonObject::default());
        Self {
            name: name.to_string(),
            tools: vec![ToolSetEntry {
                name: tool_name.to_string(),
                description: tool,
            }],
            payload_bytes,
        }
    }
}

#[async_trait::async_trait]
impl SearchableToolSet for StubSet {
    fn name(&self) -> &str {
        &self.name
    }
    fn category(&self) -> &str {
        "test"
    }
    fn category_description(&self) -> &str {
        "compose recovery test stub"
    }
    fn tools(&self) -> &[ToolSetEntry] {
        &self.tools
    }
    async fn call(
        &self,
        _subject: &AuthSubject,
        _tool_name: &str,
        _arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let filler = "x".repeat(self.payload_bytes);
        let payload = json!({ "data": filler });
        let text = serde_json::to_string(&payload).unwrap();
        let mut ctr = CallToolResult::success(vec![Content::text(text)]);
        ctr.structured_content = Some(payload);
        Ok(ctr)
    }
}

async fn build_toolsets_with_invocations(
    pool: &sqlx::PgPool,
    set: StubSet,
) -> (ToolSets, Arc<ToolInvocations>) {
    let audit = Arc::new(Audit::new(pool));
    let invocations = Arc::new(ToolInvocations::new(pool));
    let toolsets = ToolSets::init(
        ToolSetsConfig::default(),
        Some(Arc::clone(&audit)),
        None,
        Some(Arc::clone(&invocations)),
    )
    .await
    .expect("init toolsets");
    toolsets.register_searchable(set);
    (toolsets, invocations)
}

async fn insert_user(pool: &sqlx::PgPool) -> UserId {
    let id = UserId::new();
    let github_id = format!("compose-recov-{}", uuid::Uuid::from(id));
    sqlx::query("INSERT INTO users (id, github_id, created_at) VALUES ($1, $2, NOW())")
        .bind(id)
        .bind(&github_id)
        .execute(pool)
        .await
        .expect("insert user");
    id
}

fn compose_args(script: &str) -> Option<JsonObject> {
    let mut args = JsonObject::new();
    args.insert("script".to_string(), json!(script));
    Some(args)
}

fn extract_structured(result: &CallToolResult) -> &serde_json::Value {
    result
        .structured_content
        .as_ref()
        .expect("compose always emits structured_content")
}

#[tokio::test]
async fn compose_with_small_result_no_elision() {
    let pool = pool().await;
    let (toolsets, _) =
        build_toolsets_with_invocations(&pool, StubSet::new("stub", "list_items", 0)).await;

    let user_id = insert_user(&pool).await;
    let subject = AuthSubject::User(user_id);

    let result = toolsets
        .call_top_level_tool(
            &subject,
            "compose",
            compose_args("return { greeting: 'hello', n: 42 };"),
        )
        .await
        .expect("compose dispatch");

    let env = extract_structured(&result);
    assert_eq!(
        env.get("result").and_then(|v| v.get("greeting")),
        Some(&json!("hello"))
    );
    assert_eq!(env.get("result").and_then(|v| v.get("n")), Some(&json!(42)));
    assert!(
        env.get("result_invocation_id").is_none(),
        "small result must not carry a recovery handle"
    );
    let subs = env
        .get("sub_invocations")
        .and_then(|v| v.as_array())
        .unwrap();
    assert!(subs.is_empty(), "no sub-tool calls were made");
}

#[tokio::test]
async fn compose_with_large_return_curated() {
    let pool = pool().await;
    let (toolsets, invocations) =
        build_toolsets_with_invocations(&pool, StubSet::new("stub", "list_items", 0)).await;

    let user_id = insert_user(&pool).await;
    let subject = AuthSubject::User(user_id);

    let result = toolsets
        .call_top_level_tool(
            &subject,
            "compose",
            compose_args(
                "const items = []; \
                 for (let i = 0; i < 200; i++) { \
                   items.push({ id: i, blob: 'a'.repeat(100) }); \
                 } \
                 return { items };",
            ),
        )
        .await
        .expect("compose dispatch");

    let env = extract_structured(&result);
    let recovery_id = env
        .get("result_invocation_id")
        .and_then(|v| v.as_str())
        .expect("large result must carry a recovery handle");
    let recovery_uuid: uuid::Uuid = recovery_id.parse().expect("invocation_id is a uuid");

    let persisted = invocations
        .find_by_id(recovery_uuid.into())
        .await
        .expect("persisted compose row");
    assert!(
        persisted.raw_text.contains("\"items\""),
        "persisted text should be the full JSON return"
    );
    assert!(
        persisted.raw_size_bytes > 4096,
        "persisted size should be over the threshold"
    );
}

#[tokio::test]
async fn compose_sub_invocations_directory_lists_only_persisted_calls() {
    let pool = pool().await;
    let (toolsets, _) =
        build_toolsets_with_invocations(&pool, StubSet::new("stub", "list_items", 16_000)).await;

    let user_id = insert_user(&pool).await;
    let subject = AuthSubject::User(user_id);

    let result = toolsets
        .call_top_level_tool(
            &subject,
            "compose",
            compose_args(
                "const a = await tools.stub.list_items({}); \
                 const b = await tools.stub.list_items({}); \
                 const c = await tools.stub.list_items({}); \
                 return { count: 3 };",
            ),
        )
        .await
        .expect("compose dispatch");

    let env = extract_structured(&result);
    let subs = env
        .get("sub_invocations")
        .and_then(|v| v.as_array())
        .expect("sub_invocations array present");
    assert_eq!(
        subs.len(),
        3,
        "all three calls should land in the directory"
    );
    for (expected_seq, entry) in subs.iter().enumerate() {
        assert_eq!(
            entry.get("seq").and_then(|v| v.as_u64()),
            Some(expected_seq as u64),
            "sub_invocations preserve script execution order"
        );
        assert!(
            entry
                .get("invocation_id")
                .and_then(|v| v.as_str())
                .map(|s| !s.is_empty())
                .unwrap_or(false),
            "every entry has a non-empty invocation_id"
        );
        assert_eq!(
            entry.get("tool_name").and_then(|v| v.as_str()),
            Some("stub_list_items")
        );
    }
}

// WorkflowExecutor has no owner → no persistence, result is verbatim.
#[tokio::test]
async fn compose_under_workflow_executor_no_persistence() {
    let pool = pool().await;
    let (toolsets, _) =
        build_toolsets_with_invocations(&pool, StubSet::new("stub", "list_items", 16_000)).await;

    let subject = AuthSubject::workflow_executor(
        ProjectId::new(),
        WorkflowDefinitionId::new(),
        WorkflowRunId::new(),
    );

    let result = toolsets
        .call_top_level_tool(
            &subject,
            "compose",
            compose_args(
                "const r = await tools.stub.list_items({}); \
                 return { ok: true };",
            ),
        )
        .await
        .expect("compose dispatch");

    let env = extract_structured(&result);
    assert_eq!(
        env.get("result").and_then(|v| v.get("ok")),
        Some(&json!(true))
    );
    assert!(
        env.get("result_invocation_id").is_none(),
        "WorkflowExecutor must not get a recovery handle"
    );
    let subs = env
        .get("sub_invocations")
        .and_then(|v| v.as_array())
        .unwrap();
    assert!(
        subs.is_empty(),
        "sub-tool persistence skipped under WorkflowExecutor"
    );
}

#[tokio::test]
async fn tool_output_fetch_inside_compose_engine() {
    let pool = pool().await;
    let (toolsets, _) =
        build_toolsets_with_invocations(&pool, StubSet::new("stub", "list_items", 16_000)).await;

    let user_id = insert_user(&pool).await;
    let subject = AuthSubject::User(user_id);

    // Rust's `\` line-continuation collapses newlines, so a multi-line JS
    // script written with `\` ends up on one line — a leading `//` comment
    // would then swallow the rest. Use real newlines.
    let result = toolsets
        .call_top_level_tool(
            &subject,
            "compose",
            compose_args(
                "// tool_output_fetch is in the namespace.\n\
                 const present = typeof tools.tool_output_fetch === 'function';\n\
                 return { tool_output_fetch_visible: present };",
            ),
        )
        .await
        .expect("compose dispatch");

    let env = extract_structured(&result);
    assert_eq!(
        env.get("result")
            .and_then(|v| v.get("tool_output_fetch_visible")),
        Some(&json!(true)),
        "tool_output_fetch must be visible inside compose's JS engine"
    );
}
