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

/// Stub whose upstream emits a top-level JSON array as `Content::Text` —
/// mirrors `github_list_pull_requests`, `lingo_organizations_listEngines`,
/// etc. The compose JS engine should see the *unwrapped* array, not the
/// `{items, _shape}` envelope reify wraps for transport.
struct ArrayRootStub {
    name: String,
    tools: Vec<ToolSetEntry>,
    array_size: usize,
}

impl ArrayRootStub {
    fn new() -> Self {
        Self::with_size(3)
    }

    fn with_size(array_size: usize) -> Self {
        let mut tool = Tool::default();
        tool.name = "list_things".to_string().into();
        tool.description = Some("returns a top-level JSON array".to_string().into());
        tool.input_schema = Arc::new(JsonObject::default());
        Self {
            name: "stub".to_string(),
            tools: vec![ToolSetEntry {
                name: "list_things".to_string(),
                description: tool,
            }],
            array_size,
        }
    }
}

#[async_trait::async_trait]
impl SearchableToolSet for ArrayRootStub {
    fn name(&self) -> &str {
        &self.name
    }
    fn category(&self) -> &str {
        "test"
    }
    fn category_description(&self) -> &str {
        "compose unwrap test stub"
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
        let arr: Vec<_> = (0..self.array_size)
            .map(|i| json!({ "id": i, "blob": "x".repeat(200) }))
            .collect();
        let text = serde_json::to_string(&serde_json::Value::Array(arr)).unwrap();
        // Deliberately do NOT pre-populate structured_content — the
        // dispatcher's `ensure_structured_content` is responsible for
        // shape detection. Non-record upstreams flow through the text
        // channel; storage canonicalises so json modes still work.
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}

#[tokio::test]
async fn compose_top_level_array_persists_with_items_root_path() {
    // Regression pin for the Bugbot finding: when the JS script returns a
    // top-level array, the compose top-level classifier used to record
    // `root_path: "$"` because `build_walker_input` put the raw array
    // straight into `structured_content`. The fix wraps the value first so
    // `root_path_of_wrapped` detects the shape correctly. This guarantees
    // any future consumer of `invocation.root_path` for compose results
    // sees the upstream's actual shape.
    let pool = pool().await;
    let (toolsets, invocations) =
        build_toolsets_with_invocations(&pool, StubSet::new("stub", "noop", 0)).await;

    let user_id = insert_user(&pool).await;
    let subject = AuthSubject::User(user_id);

    let big = "x".repeat(200);
    let result = toolsets
        .call_top_level_tool(
            &subject,
            "compose",
            compose_args(&format!(
                "const items = []; \
                 for (let i = 0; i < 100; i++) {{ items.push({{ id: i, blob: '{}' }}); }} \
                 return items;",
                big
            )),
        )
        .await
        .expect("compose dispatch");

    let env = extract_structured(&result);
    let recovery_id = env
        .get("result_invocation_id")
        .and_then(|v| v.as_str())
        .expect("large array result must persist");
    let recovery_uuid: uuid::Uuid = recovery_id.parse().expect("uuid");

    let persisted = invocations
        .find_by_id(recovery_uuid.into())
        .await
        .expect("persisted compose row");
    assert_eq!(
        persisted.root_path, "$.items",
        "compose top-level array results must persist with root_path '$.items', \
         not the default '$' that build_walker_input's raw value would yield"
    );
}

#[tokio::test]
async fn array_root_persists_canonicalized_for_recovery_round_trip() {
    // Regression pin for the Bugbot finding (PR #323): when a non-record
    // upstream is dispatched and elided, `persist_and_envelope` /
    // `maybe_persist_sub_invocation` used to store
    // `raw.structured_content.clone()` — which is `None` for arrays after
    // the unwrap-return-channel change. The walker still emits
    // `json_array_slice` `_recover` templates for array elision, so the
    // agent following those templates would hit "json_array_slice
    // requested but invocation has no structured_content".
    //
    // This test goes through the real dispatch path (no manual seeding),
    // pulls the elided `_recover.args_template.query` out of the summary
    // the agent receives, then runs that exact query through
    // `tool_output_fetch` and asserts it actually returns the requested
    // slice instead of an error.
    let pool = pool().await;
    let audit = Arc::new(Audit::new(&pool));
    let invocations = Arc::new(ToolInvocations::new(&pool));
    let toolsets = ToolSets::init(
        ToolSetsConfig::default(),
        Some(Arc::clone(&audit)),
        None,
        Some(Arc::clone(&invocations)),
    )
    .await
    .expect("init toolsets");
    // 50 items * ~200B blob each → ~10KB+ raw, comfortably above the
    // walker's elision threshold so a `_recover` template gets emitted.
    toolsets.register_searchable(ArrayRootStub::with_size(50));

    let user_id = insert_user(&pool).await;
    let subject = AuthSubject::User(user_id);

    // 1. Dispatch the array-root stub via compose so it gets persisted.
    let dispatch = toolsets
        .call_top_level_tool(
            &subject,
            "compose",
            compose_args("return await tools.stub.list_things({});"),
        )
        .await
        .expect("compose dispatch");

    // 2. Locate the persisted sub-invocation.
    let env = extract_structured(&dispatch);
    let subs = env
        .get("sub_invocations")
        .and_then(|v| v.as_array())
        .expect("sub_invocations");
    assert_eq!(subs.len(), 1, "exactly one elided sub-call");
    let sub_id = subs[0]
        .get("invocation_id")
        .and_then(|v| v.as_str())
        .expect("sub-invocation invocation_id");

    // 3. Read back the persisted summary so we have the *real*
    //    `_recover.args_template.query` the agent would see.
    let inv_uuid: uuid::Uuid = sub_id.parse().unwrap();
    let persisted = invocations
        .find_by_id(inv_uuid.into())
        .await
        .expect("persisted row");
    assert_eq!(
        persisted.root_path, "$.items",
        "array-rooted upstream must persist root_path '$.items'"
    );
    assert!(
        persisted.original_structured.is_some(),
        "Bugbot regression: original_structured must be Some for array roots, \
         else `_recover` templates with json modes can't resolve"
    );
    assert!(
        persisted.original_structured.as_ref().unwrap().is_array(),
        "original_structured holds the unwrapped array, not an envelope"
    );

    let recover_query = persisted
        .summary
        .get("elided_paths")
        .and_then(|v| v.get(0))
        .and_then(|p| p.get("_recover"))
        .and_then(|r| r.get("args_template"))
        .and_then(|t| t.get("query"))
        .cloned()
        .expect("array elision must emit a _recover template with a query");

    // 4. Run the literal recover template and assert it actually returns
    //    the requested slice (no "no structured_content" error).
    let mut fetch_args = JsonObject::new();
    fetch_args.insert("invocation_id".to_string(), json!(sub_id));
    fetch_args.insert("query".to_string(), recover_query);
    fetch_args.insert("view".to_string(), json!("original"));
    let fetched = toolsets
        .call_top_level_tool(&subject, "tool_output_fetch", Some(fetch_args))
        .await
        .expect("recover template dispatch");

    assert!(
        !fetched.is_error.unwrap_or(false),
        "the gateway-emitted _recover template must round-trip without error"
    );
    // Slice result is a non-record (array) → goes in content channel.
    assert!(
        fetched.structured_content.is_none(),
        "array slice does NOT set structured_content (no envelope leak)"
    );
    let text: String = fetched
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        text.contains("\"id\""),
        "recovered slice text should contain the array's items"
    );
}

#[tokio::test]
async fn compose_inner_array_root_is_unwrapped_for_js() {
    let pool = pool().await;
    let audit = Arc::new(Audit::new(&pool));
    let invocations = Arc::new(ToolInvocations::new(&pool));
    let toolsets = ToolSets::init(
        ToolSetsConfig::default(),
        Some(Arc::clone(&audit)),
        None,
        Some(Arc::clone(&invocations)),
    )
    .await
    .expect("init toolsets");
    toolsets.register_searchable(ArrayRootStub::new());

    let user_id = insert_user(&pool).await;
    let subject = AuthSubject::User(user_id);

    let result = toolsets
        .call_top_level_tool(
            &subject,
            "compose",
            compose_args(
                "const r = await tools.stub.list_things({});\
                 return { isArr: Array.isArray(r), len: r.length, first_id: r[0].id };",
            ),
        )
        .await
        .expect("compose dispatch");

    let env = extract_structured(&result);
    assert_eq!(
        env.get("result").and_then(|v| v.get("isArr")),
        Some(&json!(true)),
        "compose JS must see Array.isArray(r) === true (NOT the {{items,_shape}} envelope)",
    );
    assert_eq!(
        env.get("result").and_then(|v| v.get("len")),
        Some(&json!(3))
    );
    assert_eq!(
        env.get("result").and_then(|v| v.get("first_id")),
        Some(&json!(0))
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
