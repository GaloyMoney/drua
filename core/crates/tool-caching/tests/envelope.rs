use rmcp::model::{CallToolResult, Content, RawContent};

use drua_tool_caching::{ToolCaching, ToolCachingConfig, ToolCallOwnerId};

const PG_CON: &str = "postgres://user:password@localhost:5432/drua";

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| PG_CON.to_string());
    sqlx::PgPool::connect(&url)
        .await
        .expect("connect to pg (run `make reset-deps` first)")
}

fn envelope_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn root_string_over_threshold_yields_envelope_with_recovery_section() {
    // Threshold must leave room above the ~200-byte byte-mode marker
    // overhead so head/tail blocks have non-empty content — otherwise
    // `byte_elide_string` correctly omits them and the structural
    // assertions below (which check the byte-mode envelope shape with
    // all three regions present) wouldn't fire.
    let config = ToolCachingConfig {
        generic_threshold_bytes: 512,
        ..ToolCachingConfig::default()
    };
    let caching = ToolCaching::new(&pool().await, config);

    // Stay above walker's MIN_ELIDE_BYTES floor (512) — sub-floor values
    // passthrough regardless of `generic_threshold_bytes`.
    let big = "x".repeat(1024);
    let upstream = CallToolResult::success(vec![Content::text(big.clone())]);

    let response = caching
        .maybe_summarize_and_cache(
            ToolCallOwnerId::new(),
            "bash",
            &serde_json::json!({}),
            upstream,
        )
        .await
        .expect("persistence is stubbed; this must not error");

    assert_eq!(response.elided_paths.len(), 1);

    let text = envelope_text(&response.result);

    assert!(
        text.starts_with("<summary path=\"$\" original-bytes=\""),
        "envelope must open with <summary path=\"$\" original-bytes=\"…\">; got: {text}",
    );
    assert!(text.contains("</summary>\n<recovery>\n"));
    assert!(text.trim_end().ends_with("</recovery>"));

    assert_eq!(
        text.matches("<elided path=\"$\" bytes=\"1024\">").count(),
        1,
        "exactly one rendered <elided> per elided path (single-line input → byte-mode, no lines attr)",
    );
    assert!(text.contains("tool_output_fetch("));
    // The rendered call carries a real uuid id (36 chars with 4 dashes),
    // never the pre-stamping placeholder.
    assert!(!text.contains("<this-invocation>"));
    let id_marker = "invocation_id=\"";
    let id_start = text.find(id_marker).expect("invocation_id kwarg present") + id_marker.len();
    let id_end = id_start
        + text[id_start..]
            .find('"')
            .expect("invocation_id has closing quote");
    let id = &text[id_start..id_end];
    assert_eq!(id.len(), 36, "uuid must be 36 chars; got: {id}");
    assert_eq!(
        id.matches('-').count(),
        4,
        "uuid must have 4 dashes; got: {id}"
    );
    assert!(text.contains("path=\"$\""));
    assert!(text.contains("\"mode\":\"range\""));

    assert!(text.contains("<head bytes=\""));
    assert!(text.contains("</head>"));
    assert!(text.contains("<bulk-elided original-bytes=\""));
    assert!(text.contains("</bulk-elided>"));
    assert!(text.contains("<tail bytes=\""));
    assert!(text.contains("</tail>"));
}

#[tokio::test]
async fn sub_threshold_input_is_passthrough_with_no_envelope() {
    let config = ToolCachingConfig {
        generic_threshold_bytes: 1024,
        ..ToolCachingConfig::default()
    };
    let caching = ToolCaching::new(&pool().await, config);

    let small = "just a short line".to_string();
    let upstream = CallToolResult::success(vec![Content::text(small.clone())]);

    let response = caching
        .maybe_summarize_and_cache(
            ToolCallOwnerId::new(),
            "bash",
            &serde_json::json!({}),
            upstream,
        )
        .await
        .expect("persistence is stubbed; this must not error");

    assert!(response.elided_paths.is_empty());
    let text = envelope_text(&response.result);
    assert_eq!(text, small);
    assert!(!text.contains("<summary"));
    assert!(!text.contains("<recovery"));
}

#[tokio::test]
async fn persisted_invocation_round_trips_through_find_by_id() {
    let config = ToolCachingConfig {
        generic_threshold_bytes: 64,
        ..ToolCachingConfig::default()
    };
    let caching = ToolCaching::new(&pool().await, config);

    // See note in root_string_over_threshold_… — keep above MIN_ELIDE_BYTES (512).
    let big = "x".repeat(1024);
    let upstream = CallToolResult::success(vec![Content::text(big.clone())]);
    let owner = ToolCallOwnerId::new();

    let response = caching
        .maybe_summarize_and_cache(owner, "bash", &serde_json::json!({}), upstream)
        .await
        .expect("persist must succeed");

    let id_marker = "invocation_id=\"";
    let envelope = envelope_text(&response.result);
    let id_start = envelope.find(id_marker).unwrap() + id_marker.len();
    let id_end = id_start + envelope[id_start..].find('"').unwrap();
    let inv_id: drua_tool_caching::ToolInvocationId = envelope[id_start..id_end].parse().unwrap();

    let fetched = caching
        .fetch(owner, inv_id, "$", None)
        .await
        .expect("fetch with matching owner must succeed");
    assert_eq!(envelope_text(&fetched.result), big);

    let other = ToolCallOwnerId::new();
    let err = caching.fetch(other, inv_id, "$", None).await;
    assert!(
        matches!(
            err,
            Err(drua_tool_caching::ToolCachingError::InvocationNotFound)
        ),
        "non-owner must not be able to fetch",
    );
}

#[tokio::test]
async fn no_owner_is_passthrough() {
    let caching = ToolCaching::new(&pool().await, ToolCachingConfig::default());

    let big = "x".repeat(50_000);
    let upstream = CallToolResult::success(vec![Content::text(big.clone())]);

    let response = caching
        .maybe_summarize_and_cache(None, "bash", &serde_json::json!({}), upstream)
        .await
        .expect("no-owner path must not error");

    assert!(response.elided_paths.is_empty());
    assert_eq!(envelope_text(&response.result), big);
}
