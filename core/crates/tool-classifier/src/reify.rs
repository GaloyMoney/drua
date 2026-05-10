//! Always-populate `structured_content` at the classifier boundary.
//!
//! Every dispatch path (regular MCP, compose, catalog) calls this once before
//! classification, so the universal pipeline always sees the same shape:
//! `result.structured_content` is `Some(_)`, never `None`, and always a
//! JSON **object** (record) — never a raw string, array, number, bool, or null.
//!
//! Resolution order:
//! 1. If `structured_content` is already attached → leave it alone.
//! 2. Combine `content[].text` parts.
//! 3. Try to parse the combined text as JSON.
//!    - Object → attach as-is.
//!    - Array  → attach `{ "items": [...], "_shape": "array" }`.
//!    - String → attach `{ "value": "...", "_shape": "string" }`.
//!    - Number → attach `{ "value": N, "_shape": "number" }`.
//!    - Bool   → attach `{ "value": B, "_shape": "boolean" }`.
//!    - Null   → attach `{ "value": null, "_shape": "null" }`.
//! 4. Parse failure → attach `{ "value": <raw_text>, "_shape": "string" }`.
//!
//! Why a record envelope: the MCP transport requires `structured_content` to
//! be a JSON object. Wrapping non-object values here means every downstream
//! consumer (walker, classifier, query layer, recovery template generator,
//! gateway emission) sees a record uniformly — and `tool_output_fetch` slice
//! results round-trip through the same path without breaking the wrapper.

use rmcp::model::CallToolResult;
use serde_json::{json, Value};

/// Mutates `result` so `structured_content` is `Some(record)` on return.
pub fn ensure_structured_content(result: &mut CallToolResult) {
    if result.structured_content.is_some() {
        return;
    }
    let combined = combined_text(result);
    result.structured_content = Some(reify(combined));
}

/// Wrap an arbitrary tool-output text into a record-shaped JSON value.
///
/// Pure function over a `String` so it can be unit-tested without an
/// `rmcp::CallToolResult`.
pub fn reify(text: String) -> Value {
    if text.trim().is_empty() {
        return wrap_string(text);
    }
    match serde_json::from_str::<Value>(text.trim()) {
        Ok(v) => wrap_non_record(v),
        Err(_) => wrap_string(text),
    }
}

/// Wrap a `Value` so the result is always a JSON object.
///
/// Mirror of [`reify`] for the case where the caller already has a
/// parsed `Value` (e.g. `tool_output_fetch` slice/json-mode outputs).
/// Objects pass through unchanged so the function is idempotent.
pub fn wrap_non_record(v: Value) -> Value {
    match v {
        Value::Object(_) => v,
        Value::Array(items) => json!({ "items": items, "_shape": "array" }),
        Value::String(s) => json!({ "value": s, "_shape": "string" }),
        Value::Number(n) => json!({ "value": n, "_shape": "number" }),
        Value::Bool(b) => json!({ "value": b, "_shape": "boolean" }),
        Value::Null => json!({ "value": null, "_shape": "null" }),
    }
}

fn wrap_string(s: String) -> Value {
    json!({ "value": s, "_shape": "string" })
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

    #[test]
    fn object_passes_through_unchanged() {
        let mut r = CallToolResult::success(vec![Content::text(
            r#"{"total":2,"items":[{"id":1},{"id":2}]}"#.to_string(),
        )]);
        ensure_structured_content(&mut r);
        let sc = r.structured_content.expect("structured_content set");
        assert_eq!(sc.get("total"), Some(&json!(2)));
        assert!(sc.get("items").unwrap().is_array());
        assert!(sc.get("_shape").is_none(), "objects must not be re-wrapped");
    }

    #[test]
    fn array_wraps_into_items_envelope() {
        let mut r = CallToolResult::success(vec![Content::text(
            r#"[{"a":1},{"a":2},{"a":3}]"#.to_string(),
        )]);
        ensure_structured_content(&mut r);
        let sc = r.structured_content.expect("set");
        assert!(sc.is_object(), "array must be wrapped as an object");
        assert_eq!(sc.get("_shape"), Some(&json!("array")));
        assert_eq!(sc.get("items").unwrap().as_array().unwrap().len(), 3);
    }

    #[test]
    fn skips_when_already_set() {
        let mut r = CallToolResult::success(vec![Content::text(r#"{"a":1}"#.to_string())]);
        r.structured_content = Some(json!({"existing": true}));
        ensure_structured_content(&mut r);
        assert_eq!(r.structured_content, Some(json!({"existing": true})));
    }

    #[test]
    fn non_json_text_wraps_as_string_envelope() {
        let raw = "NAME    READY   STATUS\nfoo     1/1     Running\n".to_string();
        let mut r = CallToolResult::success(vec![Content::text(raw.clone())]);
        ensure_structured_content(&mut r);
        let sc = r.structured_content.expect("set");
        assert_eq!(sc.get("_shape"), Some(&json!("string")));
        assert_eq!(sc.get("value"), Some(&Value::String(raw)));
    }

    #[test]
    fn json_string_scalar_wraps_unquoted() {
        let mut r = CallToolResult::success(vec![Content::text(r#""just a string""#.to_string())]);
        ensure_structured_content(&mut r);
        let sc = r.structured_content.expect("set");
        assert_eq!(sc.get("_shape"), Some(&json!("string")));
        assert_eq!(sc.get("value"), Some(&json!("just a string")));
    }

    #[test]
    fn json_number_wraps_as_number_envelope() {
        let mut r = CallToolResult::success(vec![Content::text("42".to_string())]);
        ensure_structured_content(&mut r);
        let sc = r.structured_content.expect("set");
        assert_eq!(sc.get("_shape"), Some(&json!("number")));
        assert_eq!(sc.get("value"), Some(&json!(42)));
    }

    #[test]
    fn json_bool_wraps_as_boolean_envelope() {
        let mut r = CallToolResult::success(vec![Content::text("true".to_string())]);
        ensure_structured_content(&mut r);
        let sc = r.structured_content.expect("set");
        assert_eq!(sc.get("_shape"), Some(&json!("boolean")));
        assert_eq!(sc.get("value"), Some(&json!(true)));
    }

    #[test]
    fn json_null_wraps_as_null_envelope() {
        let mut r = CallToolResult::success(vec![Content::text("null".to_string())]);
        ensure_structured_content(&mut r);
        let sc = r.structured_content.expect("set");
        assert_eq!(sc.get("_shape"), Some(&json!("null")));
        assert_eq!(sc.get("value"), Some(&Value::Null));
    }

    #[test]
    fn truncated_or_invalid_json_falls_back_to_raw_string() {
        let raw = r#"{"a": 1, "b":"#.to_string();
        let mut r = CallToolResult::success(vec![Content::text(raw.clone())]);
        ensure_structured_content(&mut r);
        let sc = r.structured_content.expect("set");
        assert_eq!(sc.get("_shape"), Some(&json!("string")));
        assert_eq!(sc.get("value"), Some(&Value::String(raw)));
    }

    #[test]
    fn parses_with_leading_whitespace() {
        let mut r = CallToolResult::success(vec![Content::text("  \n\t  [1,2,3]\n".to_string())]);
        ensure_structured_content(&mut r);
        let sc = r.structured_content.expect("set");
        assert_eq!(sc.get("_shape"), Some(&json!("array")));
        assert_eq!(sc.get("items").unwrap().as_array().unwrap().len(), 3);
    }

    #[test]
    fn empty_content_wraps_empty_string() {
        let mut r = CallToolResult::success(vec![]);
        ensure_structured_content(&mut r);
        let sc = r.structured_content.expect("set");
        assert_eq!(sc.get("_shape"), Some(&json!("string")));
        assert_eq!(sc.get("value"), Some(&json!("")));
    }

    #[test]
    fn wrap_non_record_object_passes_through() {
        let v = json!({"k": 1});
        assert_eq!(wrap_non_record(v.clone()), v);
    }

    #[test]
    fn wrap_non_record_array_string_number_bool_null() {
        assert_eq!(
            wrap_non_record(json!([1, 2, 3])),
            json!({"items":[1,2,3],"_shape":"array"})
        );
        assert_eq!(
            wrap_non_record(json!("hello")),
            json!({"value":"hello","_shape":"string"})
        );
        assert_eq!(
            wrap_non_record(json!(7)),
            json!({"value":7,"_shape":"number"})
        );
        assert_eq!(
            wrap_non_record(json!(false)),
            json!({"value":false,"_shape":"boolean"})
        );
        assert_eq!(
            wrap_non_record(json!(null)),
            json!({"value":null,"_shape":"null"})
        );
    }

    #[test]
    fn wrap_non_record_is_idempotent_on_envelopes() {
        let env = json!({"value":"x","_shape":"string"});
        assert_eq!(wrap_non_record(env.clone()), env);
        let arr_env = json!({"items":[1,2],"_shape":"array"});
        assert_eq!(wrap_non_record(arr_env.clone()), arr_env);
    }

    #[test]
    fn reify_helper_directly_records() {
        assert_eq!(reify(r#"{"k":1}"#.to_string()), json!({"k":1}));
        assert_eq!(
            reify("[1,2]".to_string()),
            json!({"items":[1,2],"_shape":"array"})
        );
        assert_eq!(
            reify("42".to_string()),
            json!({"value":42,"_shape":"number"})
        );
        assert_eq!(
            reify("true".to_string()),
            json!({"value":true,"_shape":"boolean"})
        );
        assert_eq!(
            reify("null".to_string()),
            json!({"value":null,"_shape":"null"})
        );
        assert_eq!(
            reify(r#""hi""#.to_string()),
            json!({"value":"hi","_shape":"string"}),
        );
        assert_eq!(
            reify("kubectl table".to_string()),
            json!({"value":"kubectl table","_shape":"string"}),
        );
    }
}
