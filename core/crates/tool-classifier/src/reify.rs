//! Classify the upstream value at the dispatch boundary so storage,
//! rendering, and recovery all key off the upstream's actual shape — without
//! injecting a transport envelope into the response the agent reads.
//!
//! Resolution order:
//! 1. If `structured_content` is already attached and is a record → leave it,
//!    root_path `$`. If it's a non-record, the upstream tool misused the
//!    channel — clear it so the agent reads the text content instead. The
//!    parsed shape still informs root_path metadata for storage.
//! 2. Otherwise, parse the combined `content[].text` body as JSON:
//!    - Object → attach as `structured_content`, root_path `$`.
//!    - Array → leave `structured_content` as `None`, root_path `$.items`.
//!    - String / Number / Bool / Null → leave `None`, root_path `$.value`.
//!    - Parse failure / non-JSON text → leave `None`, root_path `$.value`.
//!
//! Non-record upstreams (kubectl tables, code-assistant markdown, top-level
//! JSON arrays) flow to the agent through the text channel verbatim; no
//! `{value|items, _shape}` envelope leaks into the response. `root_path` is
//! recorded as invocation metadata so the persistence layer, walker, and
//! `tool_output_fetch` can reconstruct the upstream's shape on demand.

use rmcp::model::CallToolResult;
use serde_json::Value;

/// Inspect `result` and return the root_path of the upstream's value:
/// `$` for records, `$.items` for arrays, `$.value` for strings / scalars
/// / non-JSON text.
///
/// `structured_content` is *only* populated when the parse yields a JSON
/// object. Non-record values stay in the text content channel — the MCP
/// transport accepts a missing `structured_content`, and agents reading the
/// response see the upstream's verbatim text rather than a record envelope.
/// A pre-populated non-record `structured_content` is treated as upstream
/// misuse and cleared.
pub fn ensure_structured_content(result: &mut CallToolResult) -> &'static str {
    if let Some(existing) = result.structured_content.take() {
        if matches!(existing, Value::Object(_)) {
            result.structured_content = Some(existing);
            return "$";
        }
        // Non-record pre-set: clear it. root_path reflects the underlying
        // shape so storage / fetch can reconstruct.
        return root_path_of_unwrapped(&existing);
    }
    let combined = combined_text(result);
    let parsed = if combined.trim().is_empty() {
        None
    } else {
        serde_json::from_str::<Value>(combined.trim()).ok()
    };
    match parsed {
        Some(Value::Object(map)) => {
            result.structured_content = Some(Value::Object(map));
            "$"
        }
        Some(Value::Array(_)) => "$.items",
        Some(_) => "$.value",
        None => "$.value",
    }
}

/// Where would we record root_path for this *unwrapped* upstream value?
pub fn root_path_of_unwrapped(v: &Value) -> &'static str {
    match v {
        Value::Object(_) => "$",
        Value::Array(_) => "$.items",
        _ => "$.value",
    }
}

/// Resolve the upstream value to store / classify, regardless of which
/// channel the upstream used to deliver it. Returns `None` only when the
/// upstream emitted no usable shape (empty content, non-JSON text). Used by
/// the persistence layer to populate `original_structured` and by the
/// classifier to decide what to walk.
pub fn canonicalize(result: &CallToolResult) -> Option<Value> {
    if let Some(sc) = result.structured_content.as_ref() {
        return Some(sc.clone());
    }
    let combined = combined_text(result);
    if combined.trim().is_empty() {
        return None;
    }
    serde_json::from_str::<Value>(combined.trim()).ok()
}

fn combined_text(result: &CallToolResult) -> String {
    let mut combined = String::new();
    for part in &result.content {
        if let rmcp::model::RawContent::Text(t) = &part.raw {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(&t.text);
        }
    }
    combined
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::Content;
    use serde_json::json;

    #[test]
    fn object_text_sets_structured_content() {
        let mut r = CallToolResult::success(vec![Content::text(
            r#"{"total":2,"items":[{"id":1},{"id":2}]}"#.to_string(),
        )]);
        let path = ensure_structured_content(&mut r);
        assert_eq!(path, "$");
        let sc = r.structured_content.expect("set for record upstream");
        assert_eq!(sc.get("total"), Some(&json!(2)));
        assert!(sc.get("items").unwrap().is_array());
    }

    #[test]
    fn array_text_leaves_structured_content_none_with_items_root_path() {
        let mut r = CallToolResult::success(vec![Content::text(
            r#"[{"a":1},{"a":2},{"a":3}]"#.to_string(),
        )]);
        let path = ensure_structured_content(&mut r);
        assert_eq!(path, "$.items");
        assert!(
            r.structured_content.is_none(),
            "non-record upstreams must NOT set structured_content — agent reads content text",
        );
    }

    #[test]
    fn string_text_leaves_structured_content_none_with_value_root_path() {
        let raw = "NAMESPACE  KIND  NAME\nkube-system  Pod  calico\n".to_string();
        let mut r = CallToolResult::success(vec![Content::text(raw.clone())]);
        let path = ensure_structured_content(&mut r);
        assert_eq!(path, "$.value");
        assert!(r.structured_content.is_none());
        // Original text content is preserved.
        assert_eq!(combined_text(&r), raw);
    }

    #[test]
    fn json_scalar_text_leaves_none_with_value_root_path() {
        for (text, expected_path) in [
            ("42", "$.value"),
            ("true", "$.value"),
            ("null", "$.value"),
            (r#""quoted""#, "$.value"),
        ] {
            let mut r = CallToolResult::success(vec![Content::text(text.to_string())]);
            let path = ensure_structured_content(&mut r);
            assert_eq!(path, expected_path, "scalar {text}");
            assert!(r.structured_content.is_none(), "scalar {text}");
        }
    }

    #[test]
    fn invalid_json_leaves_none_with_value_root_path() {
        let raw = r#"{"a": 1, "b":"#.to_string();
        let mut r = CallToolResult::success(vec![Content::text(raw)]);
        let path = ensure_structured_content(&mut r);
        assert_eq!(path, "$.value");
        assert!(r.structured_content.is_none());
    }

    #[test]
    fn empty_content_yields_value_root_path_with_none_structured() {
        let mut r = CallToolResult::success(vec![]);
        let path = ensure_structured_content(&mut r);
        assert_eq!(path, "$.value");
        assert!(r.structured_content.is_none());
    }

    #[test]
    fn preset_record_passes_through_with_root_dollar() {
        let mut r = CallToolResult::success(vec![Content::text(r#"{"a":1}"#.to_string())]);
        r.structured_content = Some(json!({"existing": true}));
        let path = ensure_structured_content(&mut r);
        assert_eq!(path, "$");
        assert_eq!(r.structured_content, Some(json!({"existing": true})));
    }

    #[test]
    fn preset_non_record_is_cleared() {
        // Defends the transport contract: an upstream that misuses the
        // channel by stuffing a `Value::Array` directly into
        // `structured_content` would crash the wrapper validation. We clear
        // it; root_path still reflects the original shape so storage knows
        // the upstream was array-typed.
        let mut r = CallToolResult::success(vec![Content::text("[1,2,3]".to_string())]);
        r.structured_content = Some(json!([1, 2, 3]));
        let path = ensure_structured_content(&mut r);
        assert_eq!(path, "$.items");
        assert!(r.structured_content.is_none());

        let mut r = CallToolResult::success(vec![Content::text("\"hi\"".to_string())]);
        r.structured_content = Some(json!("hi"));
        let path = ensure_structured_content(&mut r);
        assert_eq!(path, "$.value");
        assert!(r.structured_content.is_none());
    }

    #[test]
    fn root_path_of_unwrapped_per_shape() {
        assert_eq!(root_path_of_unwrapped(&json!({"k":1})), "$");
        assert_eq!(root_path_of_unwrapped(&json!([1, 2])), "$.items");
        assert_eq!(root_path_of_unwrapped(&json!("hi")), "$.value");
        assert_eq!(root_path_of_unwrapped(&json!(42)), "$.value");
        assert_eq!(root_path_of_unwrapped(&json!(true)), "$.value");
        assert_eq!(root_path_of_unwrapped(&Value::Null), "$.value");
    }

    #[test]
    fn canonicalize_prefers_pre_set_structured_content() {
        let mut r = CallToolResult::success(vec![Content::text("ignored".to_string())]);
        r.structured_content = Some(json!({"k": 1}));
        assert_eq!(canonicalize(&r), Some(json!({"k": 1})));
    }

    #[test]
    fn canonicalize_parses_text_when_no_structured_content() {
        let r = CallToolResult::success(vec![Content::text(r#"[{"a":1},{"a":2}]"#.to_string())]);
        assert_eq!(canonicalize(&r), Some(json!([{"a":1},{"a":2}])));

        let r = CallToolResult::success(vec![Content::text("42".to_string())]);
        assert_eq!(canonicalize(&r), Some(json!(42)));
    }

    #[test]
    fn canonicalize_returns_none_for_non_json_or_empty() {
        let r = CallToolResult::success(vec![Content::text(
            "NAME    READY   STATUS\nfoo  1/1  Running\n".to_string(),
        )]);
        assert_eq!(canonicalize(&r), None);

        let r = CallToolResult::success(vec![]);
        assert_eq!(canonicalize(&r), None);

        let r = CallToolResult::success(vec![Content::text("   \n\t  ".to_string())]);
        assert_eq!(canonicalize(&r), None);
    }
}
