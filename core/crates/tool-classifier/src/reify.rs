//! Always-populate `structured_content` at the classifier boundary.
//!
//! Every dispatch path (regular MCP, compose, catalog) calls this once before
//! classification, so the universal pipeline always sees the same shape:
//! `result.structured_content` is `Some(record)` on return, never `None` and
//! never a raw string / array / scalar.
//!
//! Resolution order:
//! 1. If `structured_content` is already attached and is already a record →
//!    leave it alone, root_path is `$`. If it's a non-record, idempotently
//!    wrap it (so a buggy upstream that pre-populated with `Value::Array`
//!    can't escape the transport contract).
//! 2. Combine `content[].text` parts.
//! 3. Try to parse the combined text as JSON.
//!    - Object → attach as-is, root_path `$`.
//!    - Array  → attach `{ "items": [...], "_shape": "array" }`, root_path `$.items`.
//!    - String → attach `{ "value": "...", "_shape": "string" }`, root_path `$.value`.
//!    - Number → attach `{ "value": N, "_shape": "number" }`, root_path `$.value`.
//!    - Bool   → attach `{ "value": B, "_shape": "boolean" }`, root_path `$.value`.
//!    - Null   → attach `{ "value": null, "_shape": "null" }`, root_path `$.value`.
//! 4. Parse failure → attach `{ "value": <raw_text>, "_shape": "string" }`,
//!    root_path `$.value`.
//!
//! `root_path` rides alongside the wrapped value through the rest of the
//! pipeline so consumers (walker, canonical-text rendering, `tool_output_fetch`,
//! compose JS engine) can address the unwrapped upstream value directly. The
//! envelope only exists to satisfy the MCP transport's record-only
//! `structuredContent` validation.

use rmcp::model::CallToolResult;
use serde_json::{json, Value};

/// Mutates `result` so `structured_content` is `Some(record)` on return.
/// Returns the root_path: where the *unwrapped* upstream value lives within
/// the wrapped envelope (`$` for records, `$.items` for arrays, `$.value`
/// for strings / scalars / non-JSON text).
pub fn ensure_structured_content(result: &mut CallToolResult) -> &'static str {
    if let Some(existing) = result.structured_content.take() {
        let path = root_path_of_unwrapped(&existing);
        let wrapped = wrap_non_record(existing);
        result.structured_content = Some(wrapped);
        return path;
    }
    let combined = combined_text(result);
    let v = reify(combined);
    let path = root_path_of_wrapped(&v);
    result.structured_content = Some(v);
    path
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

/// Where the unwrapped value lives within a wrapped envelope. Pure shape
/// inspection — used at `ensure_structured_content` time to record the
/// root_path metadata that the rest of the pipeline keys off.
pub fn root_path_of_wrapped(v: &Value) -> &'static str {
    if let Value::Object(map) = v {
        if map.len() == 2 {
            if let Some(Value::String(shape)) = map.get("_shape") {
                match shape.as_str() {
                    "array" if map.contains_key("items") => return "$.items",
                    "string" | "number" | "boolean" | "null" if map.contains_key("value") => {
                        return "$.value"
                    }
                    _ => {}
                }
            }
        }
    }
    "$"
}

/// Inverse of `root_path_of_wrapped`: where would we wrap an unwrapped value?
pub fn root_path_of_unwrapped(v: &Value) -> &'static str {
    match v {
        Value::Object(_) => "$",
        Value::Array(_) => "$.items",
        _ => "$.value",
    }
}

/// Look through a wrapped envelope at `root_path` to the unwrapped upstream
/// value. Returns the input unchanged for `$` (records).
pub fn unwrap_at<'a>(root_path: &str, v: &'a Value) -> &'a Value {
    match root_path {
        "$" => v,
        "$.items" => v.get("items").unwrap_or(v),
        "$.value" => v.get("value").unwrap_or(v),
        _ => v,
    }
}

/// Owning version of `unwrap_at`.
pub fn unwrap_at_owned(root_path: &str, v: Value) -> Value {
    match root_path {
        "$" => v,
        "$.items" => match v {
            Value::Object(mut map) => map.remove("items").unwrap_or(Value::Object(map)),
            other => other,
        },
        "$.value" => match v {
            Value::Object(mut map) => map.remove("value").unwrap_or(Value::Object(map)),
            other => other,
        },
        _ => v,
    }
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
        let path = ensure_structured_content(&mut r);
        assert_eq!(path, "$");
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
        let path = ensure_structured_content(&mut r);
        assert_eq!(path, "$.items");
        let sc = r.structured_content.expect("set");
        assert!(sc.is_object(), "array must be wrapped as an object");
        assert_eq!(sc.get("_shape"), Some(&json!("array")));
        assert_eq!(sc.get("items").unwrap().as_array().unwrap().len(), 3);
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
    fn preset_array_idempotently_wraps() {
        // Defends the transport contract against an upstream that bypasses
        // text content and stuffs `Value::Array(_)` into `structured_content`
        // directly.
        let mut r = CallToolResult::success(vec![]);
        r.structured_content = Some(json!([1, 2, 3]));
        let path = ensure_structured_content(&mut r);
        assert_eq!(path, "$.items");
        assert_eq!(
            r.structured_content,
            Some(json!({"items":[1,2,3],"_shape":"array"})),
        );
    }

    #[test]
    fn preset_string_idempotently_wraps() {
        let mut r = CallToolResult::success(vec![]);
        r.structured_content = Some(json!("hi"));
        let path = ensure_structured_content(&mut r);
        assert_eq!(path, "$.value");
        assert_eq!(
            r.structured_content,
            Some(json!({"value":"hi","_shape":"string"})),
        );
    }

    #[test]
    fn non_json_text_wraps_as_string_envelope() {
        let raw = "NAME    READY   STATUS\nfoo     1/1     Running\n".to_string();
        let mut r = CallToolResult::success(vec![Content::text(raw.clone())]);
        let path = ensure_structured_content(&mut r);
        assert_eq!(path, "$.value");
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
    fn root_path_of_wrapped_recognises_each_envelope() {
        assert_eq!(root_path_of_wrapped(&json!({"a":1})), "$");
        assert_eq!(
            root_path_of_wrapped(&json!({"items":[1],"_shape":"array"})),
            "$.items"
        );
        assert_eq!(
            root_path_of_wrapped(&json!({"value":"x","_shape":"string"})),
            "$.value"
        );
        assert_eq!(
            root_path_of_wrapped(&json!({"value":7,"_shape":"number"})),
            "$.value"
        );
        // Object that happens to have keys named like the envelope but a
        // different `_shape` value should NOT be recognised.
        assert_eq!(
            root_path_of_wrapped(&json!({"value":"x","_shape":"custom"})),
            "$"
        );
        // Three keys ≠ envelope.
        assert_eq!(
            root_path_of_wrapped(&json!({"value":"x","_shape":"string","extra":1})),
            "$"
        );
    }

    #[test]
    fn unwrap_at_returns_inner_for_each_root_path() {
        let env = json!({"items":[1,2,3],"_shape":"array"});
        assert_eq!(unwrap_at("$.items", &env), &json!([1, 2, 3]));
        let env = json!({"value":"x","_shape":"string"});
        assert_eq!(unwrap_at("$.value", &env), &json!("x"));
        let plain = json!({"k":1});
        assert_eq!(unwrap_at("$", &plain), &plain);
    }

    #[test]
    fn unwrap_at_owned_consumes() {
        let env = json!({"items":[1,2,3],"_shape":"array"});
        assert_eq!(unwrap_at_owned("$.items", env), json!([1, 2, 3]));
        let env = json!({"value":"x","_shape":"string"});
        assert_eq!(unwrap_at_owned("$.value", env), json!("x"));
        let plain = json!({"k":1});
        assert_eq!(unwrap_at_owned("$", plain.clone()), plain);
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
