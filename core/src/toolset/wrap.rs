//! `DruaToolResult<T>` wrapper helpers — one source of truth for every
//! site that asks "is this tool's output wrapped, and how do I render it?".
//!
//! The decision is per-tool via `TopLevelTool::default_tool_caching()`:
//! tools that return `false` (`compose`, `call_tool`, `tool_output_fetch`)
//! own their own envelope shape and opt out of the wrapper. Everything
//! else, including every catalog tool, is wrapped. The text envelope is
//! built by `ToolCaching::cache`; this module covers the structured
//! channel and outputSchema advertising.
//!
//! See clat memory `019e1dbf` for the design write-up.

use serde_json::{json, Value};

/// JSON Schema fragment for the `_elided` field that
/// `ToolCallSummary::build_wire` / `build_wire_envelope` attach when the
/// walker elided something. Reused by both the `DruaToolResult` wrapper
/// schema and the envelope-owning tool augmentation so the recovery
/// metadata shape is declared in one place.
fn elided_property_schema() -> Value {
    json!({
        "type": "object",
        "description": "Present only when something was elided. Mirror of the text-channel <recovery> section.",
        "properties": {
            "invocation_id": { "type": "string" },
            "paths": {
                "type": "array",
                "items": {
                    "type": "object",
                    "description": "One elision point. total_/shown_ dimensions describe what was elided here; recover is the tool_output_fetch template that retrieves the withheld portion.",
                    "properties": {
                        "path": { "type": "string" },
                        "total_bytes": { "type": "integer" },
                        "shown_bytes": { "type": "integer" },
                        "total_lines": { "type": "integer" },
                        "shown_lines": { "type": "integer" },
                        "total_items": { "type": "integer" },
                        "shown_items": { "type": "integer" },
                        "recover": {
                            "type": "object",
                            "description": "tool_output_fetch call template"
                        }
                    },
                    "required": ["path", "total_bytes", "shown_bytes", "recover"]
                }
            }
        },
        "required": ["invocation_id", "paths"]
    })
}

/// JSON Schema fragment for the `_recovery` field — the typed recovery
/// manifest attached to every persisted invocation's structured channel.
fn recovery_property_schema() -> Value {
    json!({
        "type": "object",
        "description": "Typed recovery manifest for this cached invocation. Includes persisted-root semantics, recoverable paths, and compose sub-invocation handles when present.",
        "properties": {
            "invocation_id": { "type": "string" },
            "root_kind": { "type": "string" },
            "root_path": { "type": "string" },
            "persisted_root": { "type": "string" },
            "paths": { "type": "array" },
            "recommended_queries": { "type": "array" },
            "sub_invocations": { "type": "array" }
        },
        "required": ["invocation_id", "root_kind", "root_path", "persisted_root", "paths", "recommended_queries"]
    })
}

/// Augment an envelope-owning tool's outputSchema (a flat object schema
/// with no outer `{ result: ... }` wrapper) with the optional `_recovery`
/// and `_elided` properties that `cache_envelope()` merges into the
/// structured channel when the walker elides. Without this, strict MCP
/// clients reject elided responses because the schema is generated with
/// `additionalProperties: false`.
///
/// No-op when `schema` isn't an object schema or has no `properties` map.
/// Existing required fields are preserved — the recovery keys are
/// always optional.
pub fn merge_envelope_recovery_props(schema: &mut Value) {
    let Some(obj) = schema.as_object_mut() else {
        return;
    };
    let Some(props) = obj
        .entry("properties")
        .or_insert_with(|| json!({}))
        .as_object_mut()
    else {
        return;
    };
    props.insert("_recovery".to_string(), recovery_property_schema());
    props.insert("_elided".to_string(), elided_property_schema());
}

/// Advertise the `DruaToolResult<T>` wrapper as a tool's outputSchema —
/// matches what `cache()` actually emits on the structured
/// channel. MCP clients that validate `structuredContent` against
/// `outputSchema` need this to see the same shape.
pub fn wrap_output_schema(upstream: &Value) -> Value {
    json!({
        "type": "object",
        "properties": {
            "result": upstream,
            "_elided": elided_property_schema(),
            "_recovery": recovery_property_schema(),
        },
        "required": ["result"]
    })
}
