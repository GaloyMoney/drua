//! Regression tests for memory `019dfd6e`: sub-tool dispatches inside a
//! `compose` script must record their own audit outcome, not inherit the
//! parent compose call's outcome.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rmcp::model::{CallToolResult, Content, JsonObject, Tool};
use serde_json::json;

use drua_core::audit::{Audit, AuditEntry, AuditLogQuery};
use drua_core::auth::AuthSubject;
use drua_core::primitives::UserId;
use drua_core::toolset::{
    SearchableToolSet, ToolSetEntry, ToolSets, ToolSetsConfig, ToolSetsError,
};

const PG_CON: &str = "postgres://user:password@localhost:5432/drua";

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| PG_CON.to_string());
    sqlx::PgPool::connect(&url).await.expect("connect to pg")
}

/// Stub toolset whose single tool either echoes a fixed JSON object or
/// returns `Err`. Used to drive the compose dispatcher's audit path.
struct StubSet {
    name: String,
    tools: Vec<ToolSetEntry>,
    fail: bool,
}

impl StubSet {
    fn new(name: &str, tool_name: &str, fail: bool) -> Self {
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
            fail,
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
        "compose audit regression stub"
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
        if self.fail {
            return Err(ToolSetsError::Compose("stub upstream failure".to_string()));
        }
        let payload = json!({ "items": [{ "id": 1 }, { "id": 2 }] });
        let text = serde_json::to_string(&payload).unwrap();
        let mut ctr = CallToolResult::success(vec![Content::text(text)]);
        ctr.structured_content = Some(payload);
        Ok(ctr)
    }
}

async fn build_toolsets(pool: &sqlx::PgPool, set: StubSet) -> (ToolSets, Arc<Audit>) {
    let audit = Arc::new(Audit::new(pool));
    let toolsets = ToolSets::init(
        ToolSetsConfig::default(),
        Some(Arc::clone(&audit)),
        None,
        None,
    )
    .await
    .expect("init toolsets");
    toolsets.register_searchable(set);
    (toolsets, audit)
}

/// Audit rows are persisted via fire-and-forget `tokio::spawn` inside
/// `record_from_context`. Poll until the expected count appears.
async fn poll_audit_rows(audit: &Audit, user_id: UserId, expected: usize) -> Vec<AuditEntry> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let rows = audit
            .find(&AuditLogQuery {
                acting_user_id: Some(user_id),
                limit: 50,
                ..Default::default()
            })
            .await
            .expect("find audit rows");
        if rows.len() >= expected || Instant::now() >= deadline {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The original bug from memory `019dfd6e`: sub-tool succeeds, the JS
/// script then throws. Sub-tool's audit row must be `success`, parent
/// compose row must be `error`.
#[tokio::test]
async fn subtool_success_then_script_throw_keeps_subtool_outcome_success() {
    let pool = pool().await;
    let (toolsets, audit) = build_toolsets(&pool, StubSet::new("stub", "list_items", false)).await;

    let user_id = UserId::new();
    let subject = AuthSubject::User(user_id);

    let mut args = JsonObject::new();
    args.insert(
        "script".to_string(),
        json!(
            "const r = await tools.stub.list_items({});\
             return r.nope.find(x => x.id === 1);"
        ),
    );

    let result = toolsets
        .call_top_level_tool(&subject, "compose", Some(args))
        .await;
    assert!(
        result.is_err(),
        "compose call should propagate script error"
    );

    let rows = poll_audit_rows(&audit, user_id, 2).await;
    let sub = rows
        .iter()
        .find(|r| r.action == "compose > catalog: stub_list_items")
        .expect("sub-tool audit row");
    assert_eq!(sub.outcome, "success", "sub-tool outcome must be success");
    assert_eq!(sub.error, Some(false));
    assert!(sub.error_message.is_none());

    let parent = rows
        .iter()
        .find(|r| r.action == "compose")
        .expect("parent compose audit row");
    assert_eq!(
        parent.outcome, "error",
        "parent compose outcome must be error"
    );
    assert_eq!(parent.error, Some(true));
    assert!(parent.error_message.as_deref().is_some_and(|m| m
        .to_ascii_lowercase()
        .contains("nope")
        || m.contains("not a function")
        || m.to_ascii_lowercase().contains("undefined")));
}

/// Sub-tool returns Err. Sub-tool's audit row should be `error` with the
/// upstream message; parent compose row should also be `error` (the JS
/// engine surfaces the rejection as a thrown exception).
#[tokio::test]
async fn subtool_failure_records_error_on_subtool_row() {
    let pool = pool().await;
    let (toolsets, audit) = build_toolsets(&pool, StubSet::new("stub", "list_items", true)).await;

    let user_id = UserId::new();
    let subject = AuthSubject::User(user_id);

    let mut args = JsonObject::new();
    args.insert(
        "script".to_string(),
        json!("return await tools.stub.list_items({});"),
    );

    let result = toolsets
        .call_top_level_tool(&subject, "compose", Some(args))
        .await;
    assert!(
        result.is_err(),
        "compose call should fail when sub-tool fails"
    );

    let rows = poll_audit_rows(&audit, user_id, 2).await;
    let sub = rows
        .iter()
        .find(|r| r.action == "compose > catalog: stub_list_items")
        .expect("sub-tool audit row");
    assert_eq!(sub.outcome, "error");
    assert_eq!(sub.error, Some(true));
    assert!(sub
        .error_message
        .as_deref()
        .is_some_and(|m| m.contains("stub upstream failure")));

    let parent = rows
        .iter()
        .find(|r| r.action == "compose")
        .expect("parent compose audit row");
    assert_eq!(parent.outcome, "error");
}

/// All-success path: sub-tool succeeds and the script returns cleanly.
#[tokio::test]
async fn all_success_path_records_success_on_both_rows() {
    let pool = pool().await;
    let (toolsets, audit) = build_toolsets(&pool, StubSet::new("stub", "list_items", false)).await;

    let user_id = UserId::new();
    let subject = AuthSubject::User(user_id);

    let mut args = JsonObject::new();
    args.insert(
        "script".to_string(),
        json!("const r = await tools.stub.list_items({}); return r.items.length;"),
    );

    let result = toolsets
        .call_top_level_tool(&subject, "compose", Some(args))
        .await;
    assert!(result.is_ok(), "compose call should succeed");

    let rows = poll_audit_rows(&audit, user_id, 2).await;
    let sub = rows
        .iter()
        .find(|r| r.action == "compose > catalog: stub_list_items")
        .expect("sub-tool audit row");
    assert_eq!(sub.outcome, "success");
    assert_eq!(sub.error, Some(false));

    let parent = rows
        .iter()
        .find(|r| r.action == "compose")
        .expect("parent compose audit row");
    assert_eq!(parent.outcome, "success");
    assert_eq!(parent.error, Some(false));
}
