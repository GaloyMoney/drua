//! Integration tests for `tool_output_fetch` slice/json-mode emission.
//!
//! Locks in the contract that `tool_output_fetch` only sets
//! `structured_content` when the result is a JSON **object** (record).
//! Non-record results — text-mode slices, array slices, scalar `json_path`
//! results, string roots — flow through the `content[].text` channel
//! verbatim with `structured_content` left as `None`. The MCP transport
//! accepts a missing `structured_content`, and the agent reads the
//! upstream's actual shape rather than a `{value|items, _shape}` envelope.

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

/// Seed a record-root invocation. `original_structured` holds the parsed
/// `{items:[…], logs:"…"}` object; root_path is `$`.
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
    invocations.persist(new).await.expect("seed").id
}

/// Seed a string-root invocation: `original_structured` is `None`,
/// `raw_text` holds the verbatim upstream text, root_path is `$.value`.
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
        raw_text,
        raw_size_bytes,
        original_structured: None,
        exit_code: None,
        duration_ms: 1,
        started_at: chrono::Utc::now(),
        root_path: "$.value".to_string(),
    };
    invocations.persist(new).await.expect("seed").id
}

/// Seed an array-root invocation: `original_structured` holds the parsed
/// `Value::Array`, raw_text is the JSON encoding, root_path is `$.items`.
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

fn fetch_args(invocation_id: ToolInvocationId, query: serde_json::Value) -> Option<JsonObject> {
    let mut args = JsonObject::new();
    args.insert(
        "invocation_id".to_string(),
        json!(uuid::Uuid::from(invocation_id).to_string()),
    );
    args.insert("query".to_string(), query);
    Some(args)
}

fn text_content(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn json_array_slice_returns_array_in_text_channel_no_envelope() {
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

    assert!(
        result.structured_content.is_none(),
        "array slice must NOT set structured_content (no envelope leaks to agent)",
    );
    let text = text_content(&result);
    assert!(text.contains("\"id\": 1") || text.contains("\"id\":1"));
    assert!(text.contains("\"id\": 2") || text.contains("\"id\":2"));
}

#[tokio::test]
async fn json_path_to_array_returns_array_in_text_channel_no_envelope() {
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

    assert!(result.structured_content.is_none());
    let text = text_content(&result);
    assert!(text.contains("\"id\""));
}

#[tokio::test]
async fn json_path_to_string_returns_string_in_text_channel_no_envelope() {
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

    assert!(result.structured_content.is_none());
    assert!(text_content(&result).contains('x'));
}

#[tokio::test]
async fn json_path_to_object_sets_structured_content() {
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

    let sc = result
        .structured_content
        .expect("object slice must set structured_content");
    assert!(sc.is_object());
    assert_eq!(sc.get("id"), Some(&json!(0)));
    assert_eq!(sc.get("tag"), Some(&json!("x")));
    assert!(
        sc.get("_shape").is_none(),
        "no `_shape` envelope on records"
    );
}

#[tokio::test]
async fn grep_text_slice_returns_lines_in_text_channel_no_envelope() {
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

    assert!(result.structured_content.is_none());
    assert!(text_content(&result).contains("line-0001"));
}

#[tokio::test]
async fn head_text_slice_returns_lines_in_text_channel_no_envelope() {
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

    assert!(result.structured_content.is_none());
    assert!(text_content(&result).contains("line-0000"));
}

#[tokio::test]
async fn view_original_on_string_root_returns_raw_text_no_envelope() {
    // String-root persisted: original_structured is None, raw_text holds the
    // verbatim upstream text. view:original returns the raw text in the
    // content channel and leaves structured_content None.
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

    assert!(result.structured_content.is_none());
    let text = text_content(&result);
    assert!(text.contains("kube-system"));
    assert!(text.contains("calico"));
}

#[tokio::test]
async fn view_original_on_array_root_returns_array_text_no_envelope() {
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

    assert!(result.structured_content.is_none());
    let text = text_content(&result);
    assert!(text.starts_with('['));
    assert!(text.contains("\"id\""));
}

#[tokio::test]
async fn view_original_on_record_root_sets_structured_content() {
    let pool = pool().await;
    let (toolsets, invocations) = build(&pool).await;
    let user_id = insert_user(&pool).await;
    let subject = AuthSubject::User(user_id);
    // seed_wide creates a record-rooted invocation (root_path "$").
    let id = seed_wide(&invocations, InvocationOwner::user(user_id)).await;

    let mut args = JsonObject::new();
    args.insert(
        "invocation_id".to_string(),
        json!(uuid::Uuid::from(id).to_string()),
    );
    args.insert("query".to_string(), json!({"mode":"json_path","path":"$"}));
    let result = toolsets
        .call_top_level_tool(&subject, "tool_output_fetch", Some(args))
        .await
        .expect("dispatch");

    let sc = result
        .structured_content
        .expect("record root must set structured_content");
    assert!(sc.get("items").is_some());
    assert!(sc.get("logs").is_some());
}

#[tokio::test]
async fn json_path_dollar_on_array_root_returns_text_no_envelope() {
    // `_recover` templates emitted by the walker for an array-root
    // invocation use `path: "$"` (unwrapped space). The query resolves
    // against the unwrapped storage; the agent reads the array text
    // without any `{items, _shape}` envelope wrapping.
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

    assert!(result.structured_content.is_none());
    let text = text_content(&result);
    assert!(text.starts_with('['));
}
