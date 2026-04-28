//! Convert a JSON Schema document into a TypeScript type expression.
//!
//! Used at runtime by drua's `compose` toolset. The walker is defensive —
//! any unknown or malformed shape collapses to `any` rather than panicking,
//! since upstream MCP schemas can be arbitrary JSON Schema (draft-07,
//! draft-2020, homemade dialects, etc.).
//!
//! Two entry points:
//! - [`schema_to_ts`] — emits a complete TS type expression.
//! - [`schema_to_ts_params`] — emits the body of an object type without
//!   surrounding braces, for splicing into `function f(args: { ... })`.
//!
//! Both resolve `$ref` against the schema's own `definitions` / `$defs`
//! block with cycle detection and a [`MAX_DEPTH`] budget.

use serde_json::Value;

/// Hard recursion budget. Beyond this we emit `any` to avoid runaway walking.
pub const MAX_DEPTH: u8 = 8;

/// Convert a JSON Schema document to a TypeScript type expression. The
/// schema's own `definitions` / `$defs` block is the resolution scope for
/// any nested `$ref`s.
pub fn schema_to_ts(schema: &Value) -> String {
    let defs = extract_defs(schema);
    let mut ctx = Ctx::new(defs);
    emit(schema, &mut ctx)
}

/// Emit the body of an object type without braces, for splicing into
/// `args: { ... }`. Falls back to `"...args: any"` for non-object schemas.
pub fn schema_to_ts_params(schema: &Value) -> String {
    let defs = extract_defs(schema);
    let mut ctx = Ctx::new(defs);
    emit_object_body(schema, &mut ctx).unwrap_or_else(|| "...args: any".to_string())
}

struct Ctx<'a> {
    defs: Option<&'a serde_json::Map<String, Value>>,
    depth: u8,
    /// `$ref` paths currently being resolved; revisits emit `any`.
    visiting: Vec<String>,
}

impl<'a> Ctx<'a> {
    fn new(defs: Option<&'a serde_json::Map<String, Value>>) -> Self {
        Self {
            defs,
            depth: 0,
            visiting: Vec::new(),
        }
    }
}

/// schemars 0.8 emits `definitions` (draft-07); schemars 1.x and most modern
/// dialects emit `$defs` (draft-2020). Either key is accepted.
fn extract_defs(schema: &Value) -> Option<&serde_json::Map<String, Value>> {
    schema
        .get("$defs")
        .or_else(|| schema.get("definitions"))
        .and_then(|v| v.as_object())
}

fn emit(schema: &Value, ctx: &mut Ctx<'_>) -> String {
    if ctx.depth >= MAX_DEPTH {
        return "any".into();
    }

    // `true` is wildcard, `false` is the empty set.
    if let Some(b) = schema.as_bool() {
        return if b { "any".into() } else { "never".into() };
    }

    let obj = match schema.as_object() {
        Some(o) => o,
        None => return "any".into(),
    };

    if let Some(r) = obj.get("$ref").and_then(|v| v.as_str()) {
        return resolve_ref(r, ctx);
    }

    if let Some(arr) = obj.get("allOf").and_then(|v| v.as_array()) {
        let parts = map_subschemas(arr, ctx);
        return match parts.len() {
            0 => "any".into(),
            1 => parts.into_iter().next().unwrap(),
            _ => parts.join(" & "),
        };
    }

    for key in ["oneOf", "anyOf"] {
        if let Some(arr) = obj.get(key).and_then(|v| v.as_array()) {
            let parts = map_subschemas(arr, ctx);
            return match parts.len() {
                0 => "any".into(),
                1 => parts.into_iter().next().unwrap(),
                _ => parts.join(" | "),
            };
        }
    }

    if let Some(c) = obj.get("const") {
        return literal(c);
    }

    // Top-level enum without `type` → union of literals.
    if let Some(arr) = obj.get("enum").and_then(|v| v.as_array()) {
        if obj.get("type").is_none() {
            return enum_union(arr);
        }
    }

    let type_field = obj.get("type");
    match type_field {
        Some(Value::Array(types)) => emit_type_array(types, obj, ctx),
        Some(Value::String(s)) => emit_typed(s, obj, ctx),
        _ => {
            if obj.contains_key("properties")
                || obj.contains_key("additionalProperties")
                || obj.contains_key("required")
            {
                return emit_object(obj, ctx);
            }
            "any".into()
        }
    }
}

fn map_subschemas(arr: &[Value], ctx: &mut Ctx<'_>) -> Vec<String> {
    let mut seen: Vec<String> = Vec::with_capacity(arr.len());
    for item in arr {
        let s = emit(item, ctx);
        if !seen.contains(&s) {
            seen.push(s);
        }
    }
    seen
}

/// Supports only `#/definitions/Name` and `#/$defs/Name`; anything fancier
/// (URI, nested pointer) collapses to `any`.
fn resolve_ref(r: &str, ctx: &mut Ctx<'_>) -> String {
    if ctx.visiting.iter().any(|v| v == r) {
        return "any".into();
    }
    let name = r
        .strip_prefix("#/definitions/")
        .or_else(|| r.strip_prefix("#/$defs/"));
    let Some(name) = name else {
        return "any".into();
    };
    let Some(defs) = ctx.defs else {
        return "any".into();
    };
    let Some(target) = defs.get(name) else {
        return "any".into();
    };
    ctx.visiting.push(r.to_string());
    ctx.depth += 1;
    let out = emit(target, ctx);
    ctx.depth -= 1;
    ctx.visiting.pop();
    out
}

/// `type: [...]` — most often a nullable union, but may hold any combination
/// of primitive type names.
fn emit_type_array(
    types: &[Value],
    obj: &serde_json::Map<String, Value>,
    ctx: &mut Ctx<'_>,
) -> String {
    let names: Vec<&str> = types.iter().filter_map(|v| v.as_str()).collect();
    if names.is_empty() {
        return "any".into();
    }

    // Re-enter `emit_typed` so e.g. enum + array-type still work.
    let parts: Vec<String> = names
        .iter()
        .map(|name| emit_typed(name, obj, ctx))
        .collect();
    let mut out: Vec<String> = Vec::new();
    for p in parts {
        if !out.contains(&p) {
            out.push(p);
        }
    }
    if out.len() == 1 {
        out.into_iter().next().unwrap()
    } else {
        out.join(" | ")
    }
}

fn emit_typed(name: &str, obj: &serde_json::Map<String, Value>, ctx: &mut Ctx<'_>) -> String {
    match name {
        "string" => {
            if let Some(arr) = obj.get("enum").and_then(|v| v.as_array()) {
                enum_union(arr)
            } else {
                "string".into()
            }
        }
        "integer" | "number" => {
            if let Some(arr) = obj.get("enum").and_then(|v| v.as_array()) {
                enum_union(arr)
            } else {
                "number".into()
            }
        }
        "boolean" => "boolean".into(),
        "null" => "null".into(),
        "array" => emit_array(obj, ctx),
        "object" => emit_object(obj, ctx),
        _ => "any".into(),
    }
}

/// `items` may be missing (→ `any[]`), a single subschema (→ `T[]`), or an
/// array of subschemas (→ tuple `[A, B, C]`).
fn emit_array(obj: &serde_json::Map<String, Value>, ctx: &mut Ctx<'_>) -> String {
    match obj.get("items") {
        None => "any[]".into(),
        Some(Value::Array(arr)) => {
            ctx.depth += 1;
            let parts: Vec<String> = arr.iter().map(|s| emit(s, ctx)).collect();
            ctx.depth -= 1;
            format!("[{}]", parts.join(", "))
        }
        Some(item) => {
            ctx.depth += 1;
            let inner = emit(item, ctx);
            ctx.depth -= 1;
            // Wrap unions/intersections to keep precedence right: `(a | b)[]`.
            let needs_paren = inner.contains(' ') && !inner.starts_with('{');
            if needs_paren {
                format!("({inner})[]")
            } else {
                format!("{inner}[]")
            }
        }
    }
}

fn emit_object(obj: &serde_json::Map<String, Value>, ctx: &mut Ctx<'_>) -> String {
    let body = emit_object_body(&Value::Object(obj.clone()), ctx);
    match body {
        Some(b) if b.is_empty() => "{}".into(),
        Some(b) => format!("{{ {b} }}"),
        None => match obj.get("additionalProperties") {
            Some(Value::Bool(false)) => "{}".into(),
            Some(v) => {
                ctx.depth += 1;
                let inner = emit(v, ctx);
                ctx.depth -= 1;
                format!("Record<string, {inner}>")
            }
            None => "Record<string, any>".into(),
        },
    }
}

/// Returns `None` if the schema has no `properties`. Includes an index
/// signature when the schema also carries `additionalProperties: T`.
fn emit_object_body(schema: &Value, ctx: &mut Ctx<'_>) -> Option<String> {
    let obj = schema.as_object()?;
    let properties = obj.get("properties").and_then(|v| v.as_object())?;

    let required: Vec<&str> = obj
        .get("required")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let mut parts: Vec<String> = Vec::with_capacity(properties.len());
    for (name, prop_schema) in properties {
        ctx.depth += 1;
        let ts = emit(prop_schema, ctx);
        ctx.depth -= 1;
        let optional = if required.contains(&name.as_str()) {
            ""
        } else {
            "?"
        };
        let key = format_key(name);
        parts.push(format!("{key}{optional}: {ts}"));
    }

    if let Some(extra) = obj.get("additionalProperties") {
        if !matches!(extra, Value::Bool(false)) {
            ctx.depth += 1;
            let inner = emit(extra, ctx);
            ctx.depth -= 1;
            // Skip a redundant `[key: string]: any` when there are no other props.
            if !parts.is_empty() || inner != "any" {
                parts.push(format!("[key: string]: {inner}"));
            }
        }
    }

    Some(parts.join("; "))
}

/// Quote when the name is not a bare JS identifier (e.g. `foo-bar`, `function`).
fn format_key(name: &str) -> String {
    if is_valid_identifier(name) {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\\\""))
    }
}

fn is_valid_identifier(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_' || first == '$') {
        return false;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$') {
        return false;
    }
    !is_reserved(name)
}

fn is_reserved(name: &str) -> bool {
    matches!(
        name,
        "break"
            | "case"
            | "catch"
            | "class"
            | "const"
            | "continue"
            | "debugger"
            | "default"
            | "delete"
            | "do"
            | "else"
            | "enum"
            | "export"
            | "extends"
            | "false"
            | "finally"
            | "for"
            | "function"
            | "if"
            | "import"
            | "in"
            | "instanceof"
            | "new"
            | "null"
            | "return"
            | "super"
            | "switch"
            | "this"
            | "throw"
            | "true"
            | "try"
            | "typeof"
            | "var"
            | "void"
            | "while"
            | "with"
            | "yield"
    )
}

fn literal(v: &Value) -> String {
    match v {
        Value::String(s) => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => "null".into(),
        _ => "any".into(),
    }
}

fn enum_union(arr: &[Value]) -> String {
    if arr.is_empty() {
        return "never".into();
    }
    let mut parts: Vec<String> = Vec::with_capacity(arr.len());
    for v in arr {
        let lit = literal(v);
        if !parts.contains(&lit) {
            parts.push(lit);
        }
    }
    parts.join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ts(schema: Value) -> String {
        schema_to_ts(&schema)
    }

    // ─── Phase 1 ─────────────────────────────────────────────────────────

    #[test]
    fn primitives() {
        assert_eq!(ts(json!({"type": "string"})), "string");
        assert_eq!(ts(json!({"type": "integer"})), "number");
        assert_eq!(ts(json!({"type": "number"})), "number");
        assert_eq!(ts(json!({"type": "boolean"})), "boolean");
        assert_eq!(ts(json!({"type": "null"})), "null");
    }

    #[test]
    fn nullable_union_string() {
        // schemars 0.8: Option<String>
        assert_eq!(ts(json!({"type": ["string", "null"]})), "string | null");
    }

    #[test]
    fn nullable_union_integer() {
        // schemars 0.8: Option<i64>
        assert_eq!(
            ts(json!({"type": ["integer", "null"], "format": "int64"})),
            "number | null"
        );
    }

    #[test]
    fn string_const_enum() {
        assert_eq!(
            ts(json!({"type": "string", "enum": ["a", "b"]})),
            "\"a\" | \"b\""
        );
    }

    #[test]
    fn nested_object_recursion() {
        let s = json!({
            "type": "object",
            "properties": {
                "outer": {
                    "type": "object",
                    "properties": {
                        "inner": {"type": "string"}
                    },
                    "required": ["inner"]
                }
            },
            "required": ["outer"]
        });
        assert_eq!(ts(s), "{ outer: { inner: string } }");
    }

    #[test]
    fn record_via_additional_properties() {
        // schemars 0.8: HashMap<String, u32>
        let s = json!({
            "type": "object",
            "additionalProperties": {"type": "integer"}
        });
        assert_eq!(ts(s), "Record<string, number>");
    }

    #[test]
    fn array_of_primitives() {
        assert_eq!(
            ts(json!({"type": "array", "items": {"type": "string"}})),
            "string[]"
        );
    }

    #[test]
    fn tuple_array() {
        let s = json!({"type": "array", "items": [
            {"type": "string"},
            {"type": "integer"}
        ]});
        assert_eq!(ts(s), "[string, number]");
    }

    #[test]
    fn array_of_unions_gets_parens() {
        let s = json!({"type": "array", "items": {"type": ["string", "null"]}});
        assert_eq!(ts(s), "(string | null)[]");
    }

    #[test]
    fn optional_field() {
        let s = json!({
            "type": "object",
            "properties": {
                "a": {"type": "string"},
                "b": {"type": "integer"}
            },
            "required": ["a"]
        });
        // Field order is BTreeMap-stable in serde_json's Map.
        let out = ts(s);
        assert!(out.contains("a: string"), "{out}");
        assert!(out.contains("b?: number"), "{out}");
    }

    // ─── Phase 2 ─────────────────────────────────────────────────────────

    #[test]
    fn one_of_union() {
        let s = json!({
            "oneOf": [
                {"type": "string"},
                {"type": "integer"}
            ]
        });
        assert_eq!(ts(s), "string | number");
    }

    #[test]
    fn any_of_union() {
        let s = json!({
            "anyOf": [
                {"type": "string"},
                {"type": "boolean"}
            ]
        });
        assert_eq!(ts(s), "string | boolean");
    }

    #[test]
    fn all_of_intersection() {
        let s = json!({
            "allOf": [
                {"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"]},
                {"type": "object", "properties": {"b": {"type": "integer"}}, "required": ["b"]}
            ]
        });
        let out = ts(s);
        assert!(out.contains(" & "), "{out}");
    }

    #[test]
    fn ref_resolution_definitions() {
        let s = json!({
            "$ref": "#/definitions/Foo",
            "definitions": {
                "Foo": {"type": "string"}
            }
        });
        assert_eq!(ts(s), "string");
    }

    #[test]
    fn ref_resolution_dollar_defs() {
        let s = json!({
            "$ref": "#/$defs/Foo",
            "$defs": {
                "Foo": {"type": "integer"}
            }
        });
        assert_eq!(ts(s), "number");
    }

    #[test]
    fn ref_unknown_falls_back_to_any() {
        let s = json!({
            "$ref": "#/definitions/Missing",
            "definitions": {}
        });
        assert_eq!(ts(s), "any");
    }

    #[test]
    fn cycle_detection() {
        // Self-referential definition (Tree { children: Vec<Tree> }).
        let s = json!({
            "$ref": "#/definitions/Tree",
            "definitions": {
                "Tree": {
                    "type": "object",
                    "properties": {
                        "children": {
                            "type": "array",
                            "items": {"$ref": "#/definitions/Tree"}
                        }
                    },
                    "required": ["children"]
                }
            }
        });
        let out = ts(s);
        // Inner self-ref must collapse to `any`, outer is a real object.
        assert!(out.contains("children"), "{out}");
        assert!(out.contains("any"), "{out}");
    }

    #[test]
    fn max_depth_fallback() {
        // Build a deeply nested object that exceeds MAX_DEPTH.
        let mut s = json!({"type": "string"});
        for _ in 0..(MAX_DEPTH as usize + 4) {
            s = json!({
                "type": "object",
                "properties": {"x": s},
                "required": ["x"]
            });
        }
        let out = ts(s);
        // We don't assert exact shape, only that emission succeeds.
        assert!(!out.is_empty());
    }

    #[test]
    fn malformed_falls_back_to_any() {
        assert_eq!(ts(json!(42)), "any");
        assert_eq!(ts(json!("not a schema")), "any");
        assert_eq!(ts(json!(null)), "any");
        assert_eq!(ts(json!({})), "any");
    }

    #[test]
    fn boolean_schemas() {
        assert_eq!(ts(json!(true)), "any");
        assert_eq!(ts(json!(false)), "never");
    }

    #[test]
    fn const_literal() {
        assert_eq!(ts(json!({"const": "fixed"})), "\"fixed\"");
        assert_eq!(ts(json!({"const": 7})), "7");
        assert_eq!(ts(json!({"const": null})), "null");
    }

    #[test]
    fn schemars_tagged_enum() {
        // #[serde(tag="type")] enum E { Created, Updated }
        let s = json!({
            "oneOf": [
                {
                    "type": "object",
                    "required": ["type"],
                    "properties": {"type": {"type": "string", "enum": ["created"]}}
                },
                {
                    "type": "object",
                    "required": ["type"],
                    "properties": {"type": {"type": "string", "enum": ["updated"]}}
                }
            ]
        });
        let out = ts(s);
        assert!(out.contains("\"created\""), "{out}");
        assert!(out.contains("\"updated\""), "{out}");
        assert!(out.contains(" | "), "{out}");
    }

    #[test]
    fn quoted_key_for_hyphen() {
        let s = json!({
            "type": "object",
            "properties": {"foo-bar": {"type": "string"}},
            "required": ["foo-bar"]
        });
        assert_eq!(ts(s), "{ \"foo-bar\": string }");
    }

    #[test]
    fn quoted_key_for_reserved_word() {
        let s = json!({
            "type": "object",
            "properties": {"function": {"type": "boolean"}},
            "required": ["function"]
        });
        assert_eq!(ts(s), "{ \"function\": boolean }");
    }

    #[test]
    fn schema_to_ts_params_basic() {
        let s = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "limit": {"type": "integer"}
            },
            "required": ["name"]
        });
        let body = schema_to_ts_params(&s);
        assert!(body.contains("name: string"), "{body}");
        assert!(body.contains("limit?: number"), "{body}");
        assert!(!body.starts_with('{'));
    }

    #[test]
    fn schema_to_ts_params_no_properties_fallback() {
        assert_eq!(schema_to_ts_params(&json!({})), "...args: any");
    }

    #[test]
    fn open_object_with_props_and_additional() {
        let s = json!({
            "type": "object",
            "properties": {"a": {"type": "string"}},
            "required": ["a"],
            "additionalProperties": {"type": "integer"}
        });
        let out = ts(s);
        assert!(out.contains("a: string"), "{out}");
        assert!(out.contains("[key: string]: number"), "{out}");
    }

    #[test]
    fn empty_enum_is_never() {
        assert_eq!(ts(json!({"enum": []})), "never");
    }
}
