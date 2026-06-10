//! MCP gateway middleware: schema-driven arg coercion.
//!
//! Models and some MCP transports send args that are shaped right but typed
//! wrong: complex values JSON-encoded as strings (`query: "{\"mode\":
//! \"head\"}"` — Claude Code's transport) and scalars as strings
//! (`len: "200"` — observed from minimax-m3). Per-tool deserialization then
//! fails on what looks to the caller like a correct call.
//!
//! This middleware walks the args against the tool's input schema before
//! per-tool deserialization and fixes both in place, recursing into nested
//! objects, arrays, and `oneOf`/`anyOf`/`allOf` variants. Schema-driven so a
//! `String` field containing legitimate JSON literals (e.g. `sql: "[1,2,3]"`)
//! or numeric text stays a string.

use rmcp::model::JsonObject;
use serde_json::Value;

/// Mutates `args` in place. No-op when `schema` doesn't expose a
/// `properties` map. Records a `tracing::debug!` event when a field is
/// coerced so observability can show whether the workaround is firing.
pub(crate) fn coerce_args_to_schema(args: &mut JsonObject, schema: &Value) {
    let Some(properties) = schema.get("properties").and_then(|v| v.as_object()) else {
        return;
    };
    for (key, value) in args.iter_mut() {
        if let Some(field_schema) = properties.get(key) {
            coerce_value(value, field_schema, key);
        }
    }
}

fn coerce_value(value: &mut Value, schema: &Value, field: &str) {
    match value {
        Value::String(s) => {
            if schema_expects_complex(schema) {
                let trimmed = s.trim_start();
                if trimmed.starts_with('{') || trimmed.starts_with('[') {
                    if let Ok(parsed @ (Value::Object(_) | Value::Array(_))) =
                        serde_json::from_str::<Value>(trimmed)
                    {
                        tracing::debug!(
                            field = %field,
                            "drua_core.toolset.auto_parse_stringified_json_arg"
                        );
                        *value = parsed;
                        recurse(value, schema, field);
                        return;
                    }
                }
                // Not valid JSON or not a complex shape — leave alone;
                // per-tool deserialization surfaces a clear error.
            }
            if let Some(coerced) = coerce_scalar_string(s, schema) {
                tracing::debug!(
                    field = %field,
                    "drua_core.toolset.auto_coerce_scalar_arg"
                );
                *value = coerced;
            }
        }
        Value::Object(_) | Value::Array(_) => recurse(value, schema, field),
        _ => {}
    }
}

/// Descend into the parts of `schema` that describe `value`'s children.
/// Union variants are each tried — coercion only fires where a variant's
/// declared type is non-string, so wrong-variant visits are no-ops.
/// `$ref`s are not resolved, which also bounds recursion.
fn recurse(value: &mut Value, schema: &Value, field: &str) {
    let Some(schema_obj) = schema.as_object() else {
        return;
    };
    for key in ["oneOf", "anyOf", "allOf"] {
        if let Some(variants) = schema_obj.get(key).and_then(|v| v.as_array()) {
            for variant in variants {
                recurse(value, variant, field);
            }
        }
    }
    match value {
        Value::Object(map) => {
            if let Some(props) = schema_obj.get("properties").and_then(|v| v.as_object()) {
                for (k, v) in map.iter_mut() {
                    if let Some(field_schema) = props.get(k) {
                        coerce_value(v, field_schema, k);
                    }
                }
            }
        }
        Value::Array(items) => {
            if let Some(item_schema) = schema_obj.get("items").filter(|v| v.is_object()) {
                for item in items.iter_mut() {
                    coerce_value(item, item_schema, field);
                }
            }
        }
        _ => {}
    }
}

/// `"200"` → `200` (integer), `"1.5"` → `1.5` (number), `"true"` → `true`
/// (boolean) — only when the schema's declared type is one of those and
/// does NOT admit string.
fn coerce_scalar_string(s: &str, schema: &Value) -> Option<Value> {
    let t = schema.get("type")?;
    if type_admits_string(t) {
        return None;
    }
    let wants = |name: &str| match t {
        Value::String(v) => v == name,
        Value::Array(types) => types.iter().any(|v| v.as_str() == Some(name)),
        _ => false,
    };
    let s = s.trim();
    if wants("integer") {
        return s.parse::<i64>().ok().map(Value::from);
    }
    if wants("number") {
        return s
            .parse::<f64>()
            .ok()
            .filter(|f| f.is_finite())
            .map(Value::from);
    }
    if wants("boolean") {
        return s.parse::<bool>().ok().map(Value::from);
    }
    None
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
    fn coerces_top_level_numeric_and_boolean_strings() {
        let schema = json!({
            "properties": {
                "limit": {"type": "integer"},
                "ratio": {"type": "number"},
                "dry_run": {"type": "boolean"},
                "name": {"type": "string"},
            }
        });
        let mut a = args(json!({
            "limit": "200",
            "ratio": "1.5",
            "dry_run": "true",
            "name": "42",
        }));
        coerce_args_to_schema(&mut a, &schema);
        assert_eq!(a.get("limit").unwrap(), &json!(200));
        assert_eq!(a.get("ratio").unwrap(), &json!(1.5));
        assert_eq!(a.get("dry_run").unwrap(), &json!(true));
        assert_eq!(a.get("name").unwrap(), &json!("42"));
    }

    #[test]
    fn coerces_inside_tagged_enum_variants() {
        // The FetchQuery shape: query is anyOf [oneOf [variants...], null],
        // each variant declaring integer offset/len.
        let schema = json!({
            "properties": {
                "query": {
                    "anyOf": [
                        {
                            "oneOf": [
                                {"type": "object", "properties": {
                                    "mode": {"type": "string", "enum": ["lines"]},
                                    "offset": {"type": "integer"},
                                    "len": {"type": "integer", "minimum": 0},
                                }},
                                {"type": "object", "properties": {
                                    "mode": {"type": "string", "enum": ["range"]},
                                    "offset": {"type": "integer"},
                                    "len": {"type": "integer", "minimum": 0},
                                }},
                            ]
                        },
                        {"type": "null"}
                    ]
                }
            }
        });
        let mut a = args(json!({
            "query": {"mode": "lines", "offset": "-80", "len": "200"}
        }));
        coerce_args_to_schema(&mut a, &schema);
        assert_eq!(
            a.get("query").unwrap(),
            &json!({"mode": "lines", "offset": -80, "len": 200})
        );
    }

    #[test]
    fn coerces_inside_array_items() {
        let schema = json!({
            "properties": {
                "rows": {"type": "array", "items": {
                    "type": "object",
                    "properties": {"id": {"type": "integer"}},
                }}
            }
        });
        let mut a = args(json!({"rows": [{"id": "1"}, {"id": "2"}]}));
        coerce_args_to_schema(&mut a, &schema);
        assert_eq!(a.get("rows").unwrap(), &json!([{"id": 1}, {"id": 2}]));
    }

    #[test]
    fn coerces_after_parsing_stringified_object() {
        let schema = json!({
            "properties": {
                "query": {"type": "object", "properties": {"len": {"type": "integer"}}}
            }
        });
        let mut a = args(json!({"query": "{\"len\":\"100\"}"}));
        coerce_args_to_schema(&mut a, &schema);
        assert_eq!(a.get("query").unwrap(), &json!({"len": 100}));
    }

    #[test]
    fn leaves_non_numeric_strings_alone_for_integer_fields() {
        let schema = json!({"properties": {"limit": {"type": "integer"}}});
        let mut a = args(json!({"limit": "abc"}));
        coerce_args_to_schema(&mut a, &schema);
        assert_eq!(a.get("limit").unwrap(), &json!("abc"));
    }

    #[test]
    fn leaves_strings_alone_when_type_admits_string() {
        let schema = json!({"properties": {"id": {"type": ["string", "integer"]}}});
        let mut a = args(json!({"id": "123"}));
        coerce_args_to_schema(&mut a, &schema);
        assert_eq!(a.get("id").unwrap(), &json!("123"));
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
        coerce_args_to_schema(&mut a, &schema);
        assert_eq!(
            a.get("query").unwrap(),
            &json!({"mode": "head", "lines": 5})
        );
    }

    #[test]
    fn parses_stringified_array_into_array_field() {
        let schema = json!({"properties": {"ids": {"type": "array"}}});
        let mut a = args(json!({"ids": "[1,2,3]"}));
        coerce_args_to_schema(&mut a, &schema);
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
        coerce_args_to_schema(&mut a, &schema);
        assert_eq!(
            a.get("query").unwrap(),
            &json!({"mode": "tail", "lines": 3})
        );
    }

    #[test]
    fn leaves_string_field_alone_when_schema_says_string() {
        let schema = json!({"properties": {"sql": {"type": "string"}}});
        let mut a = args(json!({"sql": "[1,2,3]"})); // user-shaped string
        coerce_args_to_schema(&mut a, &schema);
        assert_eq!(a.get("sql").unwrap(), &json!("[1,2,3]"));
    }

    #[test]
    fn leaves_string_field_alone_when_schema_admits_string_and_null() {
        let schema = json!({"properties": {"name": {"type": ["string", "null"]}}});
        let mut a = args(json!({"name": "{\"foo\":1}"}));
        coerce_args_to_schema(&mut a, &schema);
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
        coerce_args_to_schema(&mut a, &schema);
        assert_eq!(a.get("value").unwrap(), &json!("{\"foo\":1}"));
    }

    #[test]
    fn leaves_invalid_json_alone() {
        let schema = json!({"properties": {"query": {"type": "object"}}});
        let mut a = args(json!({"query": "{not valid json"}));
        coerce_args_to_schema(&mut a, &schema);
        assert_eq!(a.get("query").unwrap(), &json!("{not valid json"));
    }

    #[test]
    fn leaves_string_not_starting_with_brace_alone() {
        let schema = json!({"properties": {"query": {"type": "object"}}});
        let mut a = args(json!({"query": "plain old string"}));
        coerce_args_to_schema(&mut a, &schema);
        assert_eq!(a.get("query").unwrap(), &json!("plain old string"));
    }

    #[test]
    fn handles_schema_without_properties() {
        let schema = json!({"type": "object"});
        let mut a = args(json!({"x": "{\"a\":1}"}));
        coerce_args_to_schema(&mut a, &schema);
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
        coerce_args_to_schema(&mut a, &schema);
        assert_eq!(a.get("known").unwrap(), &json!({"a": 1}));
        assert_eq!(a.get("unknown").unwrap(), &json!("{\"b\":2}"));
    }

    #[test]
    fn handles_stringified_with_leading_whitespace() {
        let schema = json!({"properties": {"q": {"type": "object"}}});
        let mut a = args(json!({"q": "  \n  {\"a\":1}"}));
        coerce_args_to_schema(&mut a, &schema);
        assert_eq!(a.get("q").unwrap(), &json!({"a": 1}));
    }

    #[test]
    fn does_not_parse_scalar_json() {
        // String "42" is JSON-parseable but not a complex type. Leave alone.
        let schema = json!({"properties": {"q": {"type": "object"}}});
        let mut a = args(json!({"q": "42"}));
        coerce_args_to_schema(&mut a, &schema);
        assert_eq!(a.get("q").unwrap(), &json!("42"));
    }
}
