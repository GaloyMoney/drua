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
        .cache(
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
        text.starts_with("<summary path=\"$\" total-bytes=\""),
        "envelope must open with <summary path=\"$\" total-bytes=\"…\">; got: {text}",
    );
    assert!(
        text.contains("shown-bytes=\""),
        "summary must declare shown-bytes; got: {text}",
    );
    assert!(text.contains("</summary>\n<recovery>\n"));
    assert!(text.trim_end().ends_with("</recovery>"));

    assert_eq!(
        text.matches("<elided path=\"$\" total-bytes=\"1024\"").count(),
        1,
        "exactly one rendered <elided> per elided path (single-line input → byte-mode, no lines attr)",
    );
    assert!(
        text.contains(" shown-bytes=\""),
        "elided tag must declare shown-bytes; got: {text}",
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
    assert!(text.contains("<bulk-elided bytes=\""));
    assert!(text.contains("</bulk-elided>"));
    assert!(text.contains("<tail bytes=\""));
    assert!(text.contains("</tail>"));

    // Structured channel mirrors the text envelope: {result, _elided} with
    // `result` schema-conforming (the elided string in this case) and
    // `_elided.paths` carrying the same recovery info as <recovery>.
    let structured = response
        .result
        .structured_content
        .as_ref()
        .expect("Elide mode emits structuredContent");
    assert!(structured.is_object(), "wrapper is an object: {structured}");
    assert!(
        structured.get("result").is_some(),
        "wrapper has `result`: {structured}",
    );
    let elided = structured
        .get("_elided")
        .expect("`_elided` present when something was elided");
    let inv_id = elided
        .get("invocation_id")
        .and_then(|v| v.as_str())
        .expect("`_elided.invocation_id` is a string");
    assert_eq!(inv_id.len(), 36, "uuid must be 36 chars; got: {inv_id}");
    let paths = elided
        .get("paths")
        .and_then(|v| v.as_array())
        .expect("`_elided.paths` is an array");
    assert_eq!(paths.len(), 1, "one elided path: {paths:?}");

    let recovery = structured
        .get("_recovery")
        .expect("typed `_recovery` manifest present when something was elided");
    assert_eq!(
        recovery.get("invocation_id").and_then(|v| v.as_str()),
        Some(inv_id),
        "manifest and _elided share invocation id",
    );
    assert_eq!(
        recovery.get("root_kind").and_then(|v| v.as_str()),
        Some("string")
    );
    assert_eq!(
        recovery.get("persisted_root").and_then(|v| v.as_str()),
        Some("upstream T directly; not the outer {result: ...} wire wrapper")
    );
    let recommended = recovery
        .get("recommended_queries")
        .and_then(|v| v.as_array())
        .expect("manifest exposes typed fetch templates");
    assert_eq!(recommended.len(), 1);
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
        .cache(
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

    // Sub-threshold structured channel is still wrapped in `{result: T}`
    // so the wire shape matches the advertised outputSchema regardless
    // of whether elision actually ran.
    let structured = response
        .result
        .structured_content
        .as_ref()
        .expect("Elide passthrough still wraps structuredContent");
    let result_field = structured
        .get("result")
        .expect("Elide passthrough emits {result: T}");
    assert_eq!(result_field, &serde_json::Value::String(small.clone()));
    assert!(
        structured.get("_elided").is_none(),
        "no elision → no `_elided` key",
    );
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
        .cache(owner, "bash", &serde_json::json!({}), upstream)
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
async fn persist_mode_emits_verbatim_t_without_elided_block() {
    let config = ToolCachingConfig {
        generic_threshold_bytes: 512,
        ..ToolCachingConfig::default()
    };
    let caching = ToolCaching::new(&pool().await, config);

    // Pick a value the walker WOULD elide so we hit the persist path —
    // Persist mode persists to DB but emits T verbatim on the wire (no
    // wrapper and no `_elided`); the JS engine in compose has its own size cap.
    let big = "x".repeat(1024);
    let upstream = CallToolResult::success(vec![Content::text(big.clone())]);

    let response = caching
        .persist_for_compose(
            ToolCallOwnerId::new(),
            "bash",
            &serde_json::json!({}),
            upstream,
        )
        .await
        .expect("persistence must succeed");

    assert!(
        response.invocation_id.is_some(),
        "persist mode still persists for tool_output_fetch recovery",
    );

    let structured = response
        .result
        .structured_content
        .as_ref()
        .expect("Persist mode emits structuredContent");
    assert_eq!(
        structured,
        &serde_json::Value::String(big.clone()),
        "Persist mode emits T verbatim on the wire",
    );
}

#[tokio::test]
async fn direct_and_compose_persistence_share_fetch_root_but_not_wire_wrapper() {
    let config = ToolCachingConfig {
        generic_threshold_bytes: 512,
        ..ToolCachingConfig::default()
    };
    let caching = ToolCaching::new(&pool().await, config);
    let owner = ToolCallOwnerId::new();
    let upstream_t = serde_json::json!({
        "items": (0..200).map(|i| format!("item-{i}")).collect::<Vec<_>>()
    });
    let mut upstream = CallToolResult::success(Vec::new());
    upstream.structured_content = Some(upstream_t.clone());

    let direct = caching
        .cache(owner, "contract", &serde_json::json!({}), upstream.clone())
        .await
        .expect("direct cache succeeds");
    let compose = caching
        .persist_for_compose(owner, "contract", &serde_json::json!({}), upstream)
        .await
        .expect("compose persist succeeds");

    let direct_structured = direct
        .result
        .structured_content
        .as_ref()
        .expect("direct cache has structured wrapper");
    assert!(
        direct_structured.get("result").is_some(),
        "direct MCP wire keeps DruaToolResult wrapper",
    );
    assert!(
        direct_structured.get("_recovery").is_some(),
        "direct MCP wire has typed recovery manifest",
    );

    let compose_structured = compose
        .result
        .structured_content
        .as_ref()
        .expect("compose persist has structured content");
    assert_eq!(
        compose_structured, &upstream_t,
        "compose persistence emits upstream T directly",
    );

    let fetched = caching
        .fetch(
            owner,
            compose.invocation_id.expect("compose persisted invocation"),
            "$.items",
            Some(&drua_tool_caching::FetchQuery::JsonArraySlice { offset: -2, len: 2 }),
        )
        .await
        .expect("compose persisted root is upstream T");
    assert_eq!(
        fetched.structured,
        serde_json::json!({"items": ["item-198", "item-199"]})
    );
}

#[tokio::test]
async fn no_owner_is_passthrough() {
    let caching = ToolCaching::new(&pool().await, ToolCachingConfig::default());

    let big = "x".repeat(50_000);
    let upstream = CallToolResult::success(vec![Content::text(big.clone())]);

    let response = caching
        .cache(None, "bash", &serde_json::json!({}), upstream)
        .await
        .expect("no-owner path must not error");

    assert!(response.elided_paths.is_empty());
    assert_eq!(envelope_text(&response.result), big);
}
