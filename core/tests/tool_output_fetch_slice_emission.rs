//! Integration tests for `tool_output_fetch` slice/json-mode emission.
//!
//! Locks in the contract that every `tool_output_fetch` query result lands in
//! `structured_content` as a JSON object — never a raw string, array, number,
//! bool, or null. Without this the MCP transport's record-only
//! `structuredContent` validation rejects the response and the
//! gateway-emitted `_recover` templates round-trip-fail.

use std::sync::Arc;

use rmcp::model::{CallToolResult, JsonObject};
use serde_json::json;

use drua_core::audit::Audit;
use drua_core::auth::AuthSubject;
use drua_core::primitives::UserId;
use drua_core::toolset::{ToolInvocations, ToolSets, ToolSetsConfig};
use drua_tool_cache::{InvocationOwner, NewToolInvocation, ToolInvocationId};

const PG_CON: &str = "postgres://user:password@localhost:5432/drua";

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| PG_CON.to_string());
    sqlx::PgPool::connect(&url).await.expect("connect to pg")
}

async fn build(pool: &sqlx::PgPool) -> (ToolSets, Arc<ToolInvocations>) {
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
    (toolsets, invocations)
}

async fn insert_user(pool: &sqlx::PgPool) -> UserId {
    let id = UserId::new();
    let github_id = format!("slice-emit-{}", uuid::Uuid::from(id));
    sqlx::query("INSERT INTO users (id, github_id, created_at) VALUES ($1, $2, NOW())")
        .bind(id)
        .bind(&github_id)
        .execute(pool)
        .await
        .expect("insert user");
    id
}

/// Seed a string-root invocation (root_path `$.value`, `original_structured`
/// holding a bare `Value::String`). Mirrors what reify produces when the
/// upstream emits non-JSON text like a kubectl table.
async fn seed_string_root(
    invocations: &ToolInvocations,
    owner: InvocationOwner,
) -> ToolInvocationId {
    let raw_text = "NAMESPACE  KIND  NAME\nkube-system  Pod  calico\n".to_string();
    let raw_size_bytes = raw_text.len() as i64;
    let new = NewToolInvocation {
        owner,
        tool_name: "stub_string".to_string(),
        args: json!({}),
        args_hash: vec![9, 9, 9, 9],
        classifier: "passthrough".to_string(),
        summary: json!({"kind":"passthrough","value":raw_text.clone()}),
        raw_text: raw_text.clone(),
        raw_size_bytes,
        original_structured: Some(json!(raw_text)),
        exit_code: None,
        duration_ms: 1,
        started_at: chrono::Utc::now(),
        root_path: "$.value".to_string(),
    };
    invocations.persist(new).await.expect("seed").id
}

/// Seed an array-root invocation (root_path `$.items`,
/// `original_structured` holding a bare `Value::Array`). Mirrors what reify
/// produces when the upstream emits a top-level JSON array (github
/// list-style endpoints, lingo's listers).
async fn seed_array_root(
    invocations: &ToolInvocations,
    owner: InvocationOwner,
) -> ToolInvocationId {
    let arr = json!([{"id":1},{"id":2},{"id":3}]);
    let raw_text = serde_json::to_string(&arr).unwrap();
    let raw_size_bytes = raw_text.len() as i64;
    let new = NewToolInvocation {
        owner,
        tool_name: "stub_array".to_string(),
        args: json!({}),
        args_hash: vec![8, 8, 8, 8],
        classifier: "passthrough".to_string(),
        summary: json!({"kind":"passthrough","value":arr.clone()}),
        raw_text,
        raw_size_bytes,
        original_structured: Some(arr),
        exit_code: None,
        duration_ms: 1,
        started_at: chrono::Utc::now(),
        root_path: "$.items".to_string(),
    };
    invocations.persist(new).await.expect("seed").id
}

/// Seed an invocation directly via the persistence layer so the test focuses
/// on `tool_output_fetch` emission, not on full upstream dispatch. Payload is
/// `{items:[...], logs:"..."}` — wide enough to exercise array, object,
/// string field, and raw-text grep paths.
async fn seed_wide(invocations: &ToolInvocations, owner: InvocationOwner) -> ToolInvocationId {
    let items: Vec<_> = (0..200).map(|i| json!({ "id": i, "tag": "x" })).collect();
    let logs = (0..200)
        .map(|i| format!("[line-{i:04}] kube-proxy event"))
        .collect::<Vec<_>>()
        .join("\n");
    let payload = json!({ "items": items, "logs": logs });
    let raw_text = serde_json::to_string(&payload).unwrap();
    let raw_size_bytes = raw_text.len() as i64;

    let new = NewToolInvocation {
        owner,
        tool_name: "stub_wide".to_string(),
        args: json!({}),
        args_hash: vec![1, 2, 3, 4],
        classifier: "passthrough".to_string(),
        summary: json!({"kind": "passthrough", "value": payload.clone()}),
        raw_text,
        raw_size_bytes,
        original_structured: Some(payload),
        exit_code: None,
        duration_ms: 1,
        started_at: chrono::Utc::now(),
        root_path: "$".to_string(),
    };
    let persisted = invocations.persist(new).await.expect("seed persist");
    persisted.id
}

fn fetch_args(invocation_id: ToolInvocationId, query: serde_json::Value) -> Option<JsonObject> {
    let mut args = JsonObject::new();
    args.insert(
        "invocation_id".to_string(),
        json!(uuid::Uuid::from(invocation_id).to_string()),
    );
    args.insert("query".to_string(), query);
    Some(args)
}

fn structured(result: &CallToolResult) -> &serde_json::Value {
    result
        .structured_content
        .as_ref()
        .expect("tool_output_fetch always emits structured_content")
}

#[tokio::test]
async fn json_array_slice_wraps_array_in_items_envelope() {
    let pool = pool().await;
    let (toolsets, invocations) = build(&pool).await;
    let user_id = insert_user(&pool).await;
    let subject = AuthSubject::User(user_id);
    let id = seed_wide(&invocations, InvocationOwner::user(user_id)).await;

    let result = toolsets
        .call_top_level_tool(
            &subject,
            "tool_output_fetch",
            fetch_args(
                id,
                json!({"mode":"json_array_slice","path":"$.items","offset":1,"len":2}),
            ),
        )
        .await
        .expect("dispatch");

    let sc = structured(&result);
    assert!(sc.is_object(), "json_array_slice must wrap into an object");
    assert_eq!(sc.get("_shape"), Some(&json!("array")));
    let items = sc.get("items").and_then(|v| v.as_array()).expect("items");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].get("id"), Some(&json!(1)));
    assert_eq!(items[1].get("id"), Some(&json!(2)));
}

#[tokio::test]
async fn json_path_to_array_wraps_in_items_envelope() {
    let pool = pool().await;
    let (toolsets, invocations) = build(&pool).await;
    let user_id = insert_user(&pool).await;
    let subject = AuthSubject::User(user_id);
    let id = seed_wide(&invocations, InvocationOwner::user(user_id)).await;

    let result = toolsets
        .call_top_level_tool(
            &subject,
            "tool_output_fetch",
            fetch_args(id, json!({"mode":"json_path","path":"$.items"})),
        )
        .await
        .expect("dispatch");

    let sc = structured(&result);
    assert!(sc.is_object(), "json_path → array must wrap into an object");
    assert_eq!(sc.get("_shape"), Some(&json!("array")));
    assert_eq!(
        sc.get("items").and_then(|v| v.as_array()).map(Vec::len),
        Some(200)
    );
}

#[tokio::test]
async fn json_path_to_string_wraps_in_value_envelope() {
    let pool = pool().await;
    let (toolsets, invocations) = build(&pool).await;
    let user_id = insert_user(&pool).await;
    let subject = AuthSubject::User(user_id);
    let id = seed_wide(&invocations, InvocationOwner::user(user_id)).await;

    let result = toolsets
        .call_top_level_tool(
            &subject,
            "tool_output_fetch",
            fetch_args(id, json!({"mode":"json_path","path":"$.items[0].tag"})),
        )
        .await
        .expect("dispatch");

    let sc = structured(&result);
    assert!(
        sc.is_object(),
        "json_path → string must wrap into an object"
    );
    assert_eq!(sc.get("_shape"), Some(&json!("string")));
    assert_eq!(sc.get("value"), Some(&json!("x")));
}

#[tokio::test]
async fn json_path_to_object_passes_through_unchanged() {
    let pool = pool().await;
    let (toolsets, invocations) = build(&pool).await;
    let user_id = insert_user(&pool).await;
    let subject = AuthSubject::User(user_id);
    let id = seed_wide(&invocations, InvocationOwner::user(user_id)).await;

    let result = toolsets
        .call_top_level_tool(
            &subject,
            "tool_output_fetch",
            fetch_args(id, json!({"mode":"json_path","path":"$.items[0]"})),
        )
        .await
        .expect("dispatch");

    let sc = structured(&result);
    assert!(sc.is_object(), "json_path → object must remain an object");
    assert_eq!(sc.get("id"), Some(&json!(0)));
    assert_eq!(sc.get("tag"), Some(&json!("x")));
    assert!(
        sc.get("_shape").is_none(),
        "object passthrough must not be re-wrapped",
    );
}

#[tokio::test]
async fn grep_text_slice_wraps_in_value_envelope() {
    let pool = pool().await;
    let (toolsets, invocations) = build(&pool).await;
    let user_id = insert_user(&pool).await;
    let subject = AuthSubject::User(user_id);
    let id = seed_wide(&invocations, InvocationOwner::user(user_id)).await;

    let result = toolsets
        .call_top_level_tool(
            &subject,
            "tool_output_fetch",
            fetch_args(
                id,
                json!({"mode":"grep","pattern":"line-0001","-n":true,"path":"$.logs"}),
            ),
        )
        .await
        .expect("dispatch");

    let sc = structured(&result);
    assert!(sc.is_object(), "grep slice must wrap into an object");
    assert_eq!(sc.get("_shape"), Some(&json!("string")));
    let v = sc
        .get("value")
        .and_then(|v| v.as_str())
        .expect("value is a string");
    assert!(v.contains("line-0001"));
}

#[tokio::test]
async fn view_original_wraps_string_root_for_transport() {
    // Persisted state: original_structured is a bare `Value::String`,
    // root_path is `$.value`. The fetch's view:original must reapply the
    // envelope so the response satisfies MCP's record-only structuredContent
    // contract.
    let pool = pool().await;
    let (toolsets, invocations) = build(&pool).await;
    let user_id = insert_user(&pool).await;
    let subject = AuthSubject::User(user_id);
    let id = seed_string_root(&invocations, InvocationOwner::user(user_id)).await;

    let mut args = JsonObject::new();
    args.insert(
        "invocation_id".to_string(),
        json!(uuid::Uuid::from(id).to_string()),
    );
    let result = toolsets
        .call_top_level_tool(&subject, "tool_output_fetch", Some(args))
        .await
        .expect("dispatch");

    let sc = structured(&result);
    assert!(sc.is_object(), "view:original on string root must wrap");
    assert_eq!(sc.get("_shape"), Some(&json!("string")));
    let v = sc.get("value").and_then(|v| v.as_str()).expect("value");
    assert!(v.contains("kube-system"));
}

#[tokio::test]
async fn view_original_wraps_array_root_for_transport() {
    let pool = pool().await;
    let (toolsets, invocations) = build(&pool).await;
    let user_id = insert_user(&pool).await;
    let subject = AuthSubject::User(user_id);
    let id = seed_array_root(&invocations, InvocationOwner::user(user_id)).await;

    let mut args = JsonObject::new();
    args.insert(
        "invocation_id".to_string(),
        json!(uuid::Uuid::from(id).to_string()),
    );
    let result = toolsets
        .call_top_level_tool(&subject, "tool_output_fetch", Some(args))
        .await
        .expect("dispatch");

    let sc = structured(&result);
    assert!(sc.is_object(), "view:original on array root must wrap");
    assert_eq!(sc.get("_shape"), Some(&json!("array")));
    assert_eq!(
        sc.get("items").and_then(|v| v.as_array()).map(Vec::len),
        Some(3)
    );
}

#[tokio::test]
async fn json_path_dollar_on_array_root_returns_wrapped_array() {
    // `_recover` templates emitted by the walker for an array-root
    // invocation use `path: "$"` (unwrapped space). The query must resolve
    // against the unwrapped storage and reapply the envelope.
    let pool = pool().await;
    let (toolsets, invocations) = build(&pool).await;
    let user_id = insert_user(&pool).await;
    let subject = AuthSubject::User(user_id);
    let id = seed_array_root(&invocations, InvocationOwner::user(user_id)).await;

    let result = toolsets
        .call_top_level_tool(
            &subject,
            "tool_output_fetch",
            fetch_args(id, json!({"mode":"json_path","path":"$"})),
        )
        .await
        .expect("dispatch");

    let sc = structured(&result);
    assert_eq!(sc.get("_shape"), Some(&json!("array")));
    assert_eq!(
        sc.get("items").and_then(|v| v.as_array()).map(Vec::len),
        Some(3)
    );
}

#[tokio::test]
async fn head_text_slice_wraps_in_value_envelope() {
    let pool = pool().await;
    let (toolsets, invocations) = build(&pool).await;
    let user_id = insert_user(&pool).await;
    let subject = AuthSubject::User(user_id);
    let id = seed_wide(&invocations, InvocationOwner::user(user_id)).await;

    let result = toolsets
        .call_top_level_tool(
            &subject,
            "tool_output_fetch",
            fetch_args(id, json!({"mode":"head","lines":3,"path":"$.logs"})),
        )
        .await
        .expect("dispatch");

    let sc = structured(&result);
    assert!(sc.is_object(), "head slice must wrap into an object");
    assert_eq!(sc.get("_shape"), Some(&json!("string")));
    let v = sc
        .get("value")
        .and_then(|v| v.as_str())
        .expect("value is a string");
    assert!(v.contains("line-0000"));
}
