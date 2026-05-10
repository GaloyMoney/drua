//! Always-populate `structured_content` at the classifier boundary.
//!
//! Every dispatch path (regular MCP, compose, catalog) calls this once before
//! classification, so the universal pipeline always sees the same shape:
//! `result.structured_content` is `Some(_)`, never `None`.
//!
//! Resolution order:
//! 1. If `structured_content` is already attached → leave it alone.
//! 2. Combine `content[].text` parts.
//! 3. If the combined text parses as a JSON object or array → attach the
//!    parsed value.
//! 4. Otherwise → attach `Value::String(combined_text)` so the walker, query
//!    layer, and downstream consumers always have a structured handle.
//!
//! Net: agents and JS consumers (compose) never have to branch on "did the
//! tool populate structured_content?" — the answer is always yes.

use rmcp::model::CallToolResult;
use serde_json::Value;

/// Mutates `result` so `structured_content` is `Some(_)` on return.
pub fn ensure_structured_content(result: &mut CallToolResult) {
    if result.structured_content.is_some() {
        return;
    }
    let combined = combined_text(result);
    let parsed = parse_json_object_or_array(&combined);
    result.structured_content = Some(parsed.unwrap_or(Value::String(combined)));
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

fn parse_json_object_or_array(text: &str) -> Option<Value> {
    let trimmed = text.trim_start();
    if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
        return None;
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(v @ (Value::Object(_) | Value::Array(_))) => Some(v),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::Content;

    #[test]
    fn ensure_object_text_into_structured_content() {
        let mut r = CallToolResult::success(vec![Content::text(
            r#"{"total":2,"items":[{"id":1},{"id":2}]}"#.to_string(),
        )]);
        ensure_structured_content(&mut r);
        let sc = r.structured_content.expect("structured_content set");
        assert_eq!(sc.get("total"), Some(&serde_json::json!(2)));
        assert!(sc.get("items").unwrap().is_array());
    }

    #[test]
    fn ensure_array_text_into_structured_content() {
        let mut r = CallToolResult::success(vec![Content::text(
            r#"[{"a":1},{"a":2},{"a":3}]"#.to_string(),
        )]);
        ensure_structured_content(&mut r);
        let sc = r.structured_content.expect("structured_content set");
        assert!(sc.is_array());
        assert_eq!(sc.as_array().unwrap().len(), 3);
    }

    #[test]
    fn ensure_skips_when_already_set() {
        let mut r = CallToolResult::success(vec![Content::text(r#"{"a":1}"#.to_string())]);
        r.structured_content = Some(serde_json::json!({"existing": true}));
        ensure_structured_content(&mut r);
        assert_eq!(
            r.structured_content,
            Some(serde_json::json!({"existing": true}))
        );
    }

    #[test]
    fn ensure_falls_back_to_value_string_for_non_json_text() {
        let mut r = CallToolResult::success(vec![Content::text(
            "NAME    READY   STATUS\nfoo     1/1     Running\n".to_string(),
        )]);
        ensure_structured_content(&mut r);
        let sc = r.structured_content.expect("set");
        assert_eq!(
            sc,
            Value::String("NAME    READY   STATUS\nfoo     1/1     Running\n".to_string())
        );
    }

    #[test]
    fn ensure_falls_back_to_value_string_for_scalar_json() {
        // JSON scalars like `42` or `"foo"` are valid JSON but not "structured" —
        // wrap them as Value::String so the walker has a uniform input.
        let mut r = CallToolResult::success(vec![Content::text("42".to_string())]);
        ensure_structured_content(&mut r);
        assert_eq!(r.structured_content, Some(Value::String("42".to_string())));

        let mut r = CallToolResult::success(vec![Content::text(r#""just a string""#.to_string())]);
        ensure_structured_content(&mut r);
        assert_eq!(
            r.structured_content,
            Some(Value::String(r#""just a string""#.to_string()))
        );
    }

    #[test]
    fn ensure_falls_back_for_truncated_or_invalid_json() {
        let mut r = CallToolResult::success(vec![Content::text(r#"{"a": 1, "b":"#.to_string())]);
        ensure_structured_content(&mut r);
        assert_eq!(
            r.structured_content,
            Some(Value::String(r#"{"a": 1, "b":"#.to_string()))
        );
    }

    #[test]
    fn ensure_parses_with_leading_whitespace() {
        let mut r = CallToolResult::success(vec![Content::text("  \n\t  [1,2,3]\n".to_string())]);
        ensure_structured_content(&mut r);
        assert!(r.structured_content.unwrap().is_array());
    }

    #[test]
    fn ensure_falls_back_to_empty_string_when_no_text() {
        let mut r = CallToolResult::success(vec![]);
        ensure_structured_content(&mut r);
        assert_eq!(r.structured_content, Some(Value::String(String::new())));
    }
}
