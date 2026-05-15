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

/// Advertise the `DruaToolResult<T>` wrapper as a tool's outputSchema —
/// matches what `cache()` actually emits on the structured
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
            }
        },
        "required": ["result"]
    })
}
