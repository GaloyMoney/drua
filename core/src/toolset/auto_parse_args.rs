//! MCP gateway middleware: schema-driven auto-parse of stringified JSON args.
//!
//! Some MCP clients (Claude Code's transport, in particular) JSON-encode
//! complex nested arg values as strings — e.g. they send `query: "{\"mode\":
//! \"head\"}"` instead of `query: {"mode": "head"}`. Per-tool deserialization
//! then fails with "expected internally tagged enum FetchQuery" on what
//! looks to the caller like a correctly-shaped object.
//!
//! This middleware walks the args once before per-tool deserialization and,
//! for each arg whose schema declares an object/array/enum type, tries to
//! parse a JSON-shaped string in place. Schema-driven so a `String` field
//! containing legitimate JSON literals (e.g. `sql: "[1,2,3]"`) stays a string.

use rmcp::model::JsonObject;
use serde_json::Value;

/// Walks `args` and replaces stringified JSON in fields where `schema` declares
/// a non-string shape. Mutates `args` in place. No-op when `schema` doesn't
/// expose a `properties` map. Records a `tracing::debug!` event when a field
/// is auto-parsed so observability can show whether the workaround is firing.
pub(crate) fn auto_parse_stringified_json_args(args: &mut JsonObject, schema: &Value) {
    let Some(properties) = schema.get("properties").and_then(|v| v.as_object()) else {
        return;
    };
    for (key, value) in args.iter_mut() {
        let Some(field_schema) = properties.get(key) else {
            continue;
        };
        if !schema_expects_complex(field_schema) {
            continue;
        }
        let Value::String(s) = value else {
            continue;
        };
        let trimmed = s.trim_start();
        if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
            continue;
        }
        match serde_json::from_str::<Value>(trimmed) {
            Ok(parsed @ (Value::Object(_) | Value::Array(_))) => {
                tracing::debug!(
                    field = %key,
                    "drua_core.toolset.auto_parse_stringified_json_arg"
                );
                *value = parsed;
            }
            _ => {
                // Not valid JSON or not a complex shape — leave alone; per-tool
                // deserialization will surface a clear error to the caller.
            }
        }
    }
}

/// True when `schema` declares an object, array, or sum-of-objects (enum)
/// shape. False when it explicitly admits `string` (including `["string",
/// "null"]`-style unions).
fn schema_expects_complex(schema: &Value) -> bool {
    let Some(obj) = schema.as_object() else {
        return false;
    };
    if let Some(t) = obj.get("type") {
        if type_admits_string(t) {
            return false;
        }
        if type_admits_object_or_array(t) {
            return true;
        }
    }
    // schemars emits `oneOf` / `anyOf` for tagged enums and union types.
    // If any variant admits string, treat the whole field as string-permissive
    // (don't auto-parse). Otherwise if any variant is an object/array, treat
    // as complex.
    for key in ["oneOf", "anyOf", "allOf"] {
        if let Some(variants) = obj.get(key).and_then(|v| v.as_array()) {
            if variants.iter().any(schema_admits_string) {
                return false;
            }
            if variants.iter().any(schema_expects_complex) {
                return true;
            }
        }
    }
    false
}

fn schema_admits_string(schema: &Value) -> bool {
    let Some(obj) = schema.as_object() else {
        return false;
    };
    obj.get("type").map(type_admits_string).unwrap_or(false)
}

fn type_admits_string(t: &Value) -> bool {
    match t {
        Value::String(s) => s == "string",
        Value::Array(types) => types
            .iter()
            .any(|v| v.as_str().map(|s| s == "string").unwrap_or(false)),
        _ => false,
    }
}

fn type_admits_object_or_array(t: &Value) -> bool {
    match t {
        Value::String(s) => s == "object" || s == "array",
        Value::Array(types) => types.iter().any(|v| {
            v.as_str()
                .map(|s| s == "object" || s == "array")
                .unwrap_or(false)
        }),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(v: serde_json::Value) -> JsonObject {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn parses_stringified_object_into_object_field() {
        let schema = json!({
            "properties": {
                "query": {"type": "object", "properties": {"mode": {"type": "string"}}}
            }
        });
        let mut a = args(json!({
            "query": "{\"mode\":\"head\",\"lines\":5}"
        }));
        auto_parse_stringified_json_args(&mut a, &schema);
        assert_eq!(
            a.get("query").unwrap(),
            &json!({"mode": "head", "lines": 5})
        );
    }

    #[test]
    fn parses_stringified_array_into_array_field() {
        let schema = json!({"properties": {"ids": {"type": "array"}}});
        let mut a = args(json!({"ids": "[1,2,3]"}));
        auto_parse_stringified_json_args(&mut a, &schema);
        assert_eq!(a.get("ids").unwrap(), &json!([1, 2, 3]));
    }

    #[test]
    fn parses_oneof_object_variants_as_complex() {
        // schemars emits this shape for tagged enums like `FetchQuery`.
        let schema = json!({
            "properties": {
                "query": {
                    "oneOf": [
                        {"type": "object", "properties": {"mode": {"const": "tail"}}},
                        {"type": "object", "properties": {"mode": {"const": "head"}}},
                    ]
                }
            }
        });
        let mut a = args(json!({
            "query": "{\"mode\":\"tail\",\"lines\":3}"
        }));
        auto_parse_stringified_json_args(&mut a, &schema);
        assert_eq!(
            a.get("query").unwrap(),
            &json!({"mode": "tail", "lines": 3})
        );
    }

    #[test]
    fn leaves_string_field_alone_when_schema_says_string() {
        let schema = json!({"properties": {"sql": {"type": "string"}}});
        let mut a = args(json!({"sql": "[1,2,3]"})); // user-shaped string
        auto_parse_stringified_json_args(&mut a, &schema);
        assert_eq!(a.get("sql").unwrap(), &json!("[1,2,3]"));
    }

    #[test]
    fn leaves_string_field_alone_when_schema_admits_string_and_null() {
        let schema = json!({"properties": {"name": {"type": ["string", "null"]}}});
        let mut a = args(json!({"name": "{\"foo\":1}"}));
        auto_parse_stringified_json_args(&mut a, &schema);
        assert_eq!(a.get("name").unwrap(), &json!("{\"foo\":1}"));
    }

    #[test]
    fn leaves_oneof_with_string_variant_alone() {
        let schema = json!({
            "properties": {
                "value": {
                    "oneOf": [{"type": "string"}, {"type": "object"}]
                }
            }
        });
        let mut a = args(json!({"value": "{\"foo\":1}"}));
        auto_parse_stringified_json_args(&mut a, &schema);
        assert_eq!(a.get("value").unwrap(), &json!("{\"foo\":1}"));
    }

    #[test]
    fn leaves_invalid_json_alone() {
        let schema = json!({"properties": {"query": {"type": "object"}}});
        let mut a = args(json!({"query": "{not valid json"}));
        auto_parse_stringified_json_args(&mut a, &schema);
        assert_eq!(a.get("query").unwrap(), &json!("{not valid json"));
    }

    #[test]
    fn leaves_string_not_starting_with_brace_alone() {
        let schema = json!({"properties": {"query": {"type": "object"}}});
        let mut a = args(json!({"query": "plain old string"}));
        auto_parse_stringified_json_args(&mut a, &schema);
        assert_eq!(a.get("query").unwrap(), &json!("plain old string"));
    }

    #[test]
    fn handles_schema_without_properties() {
        let schema = json!({"type": "object"});
        let mut a = args(json!({"x": "{\"a\":1}"}));
        auto_parse_stringified_json_args(&mut a, &schema);
        // No-op — without `properties` we have no per-field schema.
        assert_eq!(a.get("x").unwrap(), &json!("{\"a\":1}"));
    }

    #[test]
    fn ignores_fields_not_in_schema() {
        let schema = json!({"properties": {"known": {"type": "object"}}});
        let mut a = args(json!({
            "known": "{\"a\":1}",
            "unknown": "{\"b\":2}",
        }));
        auto_parse_stringified_json_args(&mut a, &schema);
        assert_eq!(a.get("known").unwrap(), &json!({"a": 1}));
        assert_eq!(a.get("unknown").unwrap(), &json!("{\"b\":2}"));
    }

    #[test]
    fn handles_stringified_with_leading_whitespace() {
        let schema = json!({"properties": {"q": {"type": "object"}}});
        let mut a = args(json!({"q": "  \n  {\"a\":1}"}));
        auto_parse_stringified_json_args(&mut a, &schema);
        assert_eq!(a.get("q").unwrap(), &json!({"a": 1}));
    }

    #[test]
    fn does_not_parse_scalar_json() {
        // String "42" is JSON-parseable but not a complex type. Leave alone.
        let schema = json!({"properties": {"q": {"type": "object"}}});
        let mut a = args(json!({"q": "42"}));
        auto_parse_stringified_json_args(&mut a, &schema);
        assert_eq!(a.get("q").unwrap(), &json!("42"));
    }
}
