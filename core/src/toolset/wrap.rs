//! `DruaToolResult<T>` wrapper helpers — one source of truth for every
//! site that asks "is this tool's output wrapped, and how do I render it?".
//!
//! The decision is per-tool via `TopLevelTool::default_tool_caching()`:
//! tools that return `false` (`compose`, `call_tool`, `tool_output_fetch`)
//! own their own envelope shape and opt out of the wrapper. Everything
//! else, including every catalog tool, is wrapped. The text envelope is
//! built by `ToolCaching::cache`; this module covers the structured
//! channel, outputSchema advertising, and the inline JS-engine wrap.
//!
//! See clat memory `019e1dbf` for the design write-up.

use serde_json::{json, Value};

/// Advertise the `DruaToolResult<T>` wrapper as a tool's outputSchema —
/// matches what `cache(WrapMode::Elide)` actually emits on the structured
/// channel. MCP clients that validate `structuredContent` against
/// `outputSchema` need this to see the same shape.
pub fn wrap_output_schema(upstream: &Value) -> Value {
    json!({
        "type": "object",
        "properties": {
            "result": upstream,
            "_elided": {
                "type": "object",
                "description": "Present only when something was elided. Mirror of the text-channel <recovery> section.",
                "properties": {
                    "invocation_id": { "type": "string" },
                    "paths": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string" },
                                "bytes": { "type": "integer" },
                                "lines": { "type": "integer" },
                                "length": { "type": "integer" },
                                "head_count": {
                                    "type": "integer",
                                    "description": "Number of leading items present in `result`. Items `[head_count..length)` are recoverable via `recover`."
                                },
                                "recover": {
                                    "type": "object",
                                    "description": "tool_output_fetch call template"
                                }
                            },
                            "required": ["path", "bytes", "recover"]
                        }
                    }
                },
                "required": ["invocation_id", "paths"]
            }
        },
        "required": ["result"]
    })
}

/// Wrap a TS type into `{ result: T }` — the compose-script-visible shape.
/// Compose sub-dispatch is `WrapMode::Persist`, so `_elided` is always
/// absent; the TS signature stays a single field.
pub fn wrap_output_ts(inner_ts: &str) -> String {
    format!("{{ result: {inner_ts} }}")
}

/// Wrap a `serde_json::Value` for the JS engine's view of a cached
/// sub-call result.
pub fn wrap_value(value: Value) -> Value {
    json!({ "result": value })
}
