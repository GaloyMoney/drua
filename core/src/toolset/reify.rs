//! Reifies JSON-as-text into `structured_content` for upstream tool results.
//!
//! Many upstream MCP servers return their result as a JSON string in
//! `content[].text` without populating `structured_content`. The universal
//! pipeline (classifier, walker, recovery queries) operates structurally on
//! `structured_content`, so without reify the walker treats the whole payload
//! as a single `Value::String` and falls back to byte-level elision —
//! losing the JSON-aware features (per-key peeling, array sentinels,
//! `_recover` templates).
//!
//! This pass runs once per upstream call result, before classification, for
//! every toolset class — not just `UpstreamToolSet` — so deployment-proxy
//! and other custom toolsets benefit too.

use rmcp::model::CallToolResult;

pub(crate) fn reify_json_structured_content(result: &mut CallToolResult) {
    if result.structured_content.is_some() {
        return;
    }
    let mut combined = String::new();
    for part in &result.content {
        if let rmcp::model::RawContent::Text(t) = &part.raw {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(&t.text);
        }
    }
    let trimmed = combined.trim_start();
    if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
        return;
    }
    if let Ok(v @ (serde_json::Value::Object(_) | serde_json::Value::Array(_))) =
        serde_json::from_str::<serde_json::Value>(trimmed)
    {
        result.structured_content = Some(v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::Content;

    #[test]
    fn reify_object_text_into_structured_content() {
        let mut r = CallToolResult::success(vec![Content::text(
            r#"{"total":2,"items":[{"id":1},{"id":2}]}"#.to_string(),
        )]);
        reify_json_structured_content(&mut r);
        let sc = r.structured_content.expect("structured_content set");
        assert_eq!(sc.get("total"), Some(&serde_json::json!(2)));
        assert!(sc.get("items").unwrap().is_array());
    }

    #[test]
    fn reify_array_text_into_structured_content() {
        let mut r = CallToolResult::success(vec![Content::text(
            r#"[{"a":1},{"a":2},{"a":3}]"#.to_string(),
        )]);
        reify_json_structured_content(&mut r);
        let sc = r.structured_content.expect("structured_content set");
        assert!(sc.is_array());
        assert_eq!(sc.as_array().unwrap().len(), 3);
    }

    #[test]
    fn reify_skips_when_structured_content_already_set() {
        let mut r = CallToolResult::success(vec![Content::text(r#"{"a":1}"#.to_string())]);
        r.structured_content = Some(serde_json::json!({"existing": true}));
        reify_json_structured_content(&mut r);
        assert_eq!(
            r.structured_content,
            Some(serde_json::json!({"existing": true}))
        );
    }

    #[test]
    fn reify_skips_non_json_text() {
        let mut r = CallToolResult::success(vec![Content::text(
            "NAME    READY   STATUS\nfoo     1/1     Running\n".to_string(),
        )]);
        reify_json_structured_content(&mut r);
        assert!(r.structured_content.is_none());
    }

    #[test]
    fn reify_skips_scalar_json() {
        let mut r = CallToolResult::success(vec![Content::text("42".to_string())]);
        reify_json_structured_content(&mut r);
        assert!(r.structured_content.is_none());

        let mut r = CallToolResult::success(vec![Content::text(r#""just a string""#.to_string())]);
        reify_json_structured_content(&mut r);
        assert!(r.structured_content.is_none());
    }

    #[test]
    fn reify_skips_truncated_or_invalid_json() {
        let mut r = CallToolResult::success(vec![Content::text(r#"{"a": 1, "b":"#.to_string())]);
        reify_json_structured_content(&mut r);
        assert!(r.structured_content.is_none());
    }

    #[test]
    fn reify_handles_leading_whitespace() {
        let mut r = CallToolResult::success(vec![Content::text("  \n\t  [1,2,3]\n".to_string())]);
        reify_json_structured_content(&mut r);
        assert!(r.structured_content.unwrap().is_array());
    }
}
