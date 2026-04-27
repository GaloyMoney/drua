//! `compose` — execute JavaScript that chains multiple MCP tool calls in a
//! single round trip. The script has access to a `tools` proxy; each
//! `tools.prefixed_name(args)` call dispatches to the backing catalog toolset
//! and is individually audit-logged.
//!
//! Includes TypeScript declaration generation from catalog JSON Schemas so
//! agents see typed APIs in the execution context.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, RwLock};
use std::time::Duration;

use rmcp::model::{CallToolResult, Content, JsonObject};
use serde::Deserialize;

use crate::audit::Audit;
use crate::auth::AuthSubject;

use super::super::error::ToolSetsError;
use super::super::filter::OutputFilter;
use super::super::traits::{SearchableToolSet, TopLevelTool};
use super::liberal;
use super::{parse_params, schema_for};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_TIMEOUT: Duration = Duration::from_secs(300);

// ---------------------------------------------------------------------------
// Params
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
struct ComposeParams {
    /// JavaScript code to execute. Has access to a `tools` namespace for
    /// calling upstream MCP tools. Use `return` for the final value.
    /// Top-level `await` is supported.
    script: String,

    /// Optional execution timeout in milliseconds. Defaults to 120 000 (2 min),
    /// max 300 000 (5 min). Covers the entire script including all tool calls.
    #[serde(default, deserialize_with = "liberal::deserialize_option_i64")]
    timeout_ms: Option<i64>,
}

/// A `TopLevelTool` that evaluates JavaScript with access to upstream MCP
/// tools via a `tools.*` proxy. Each inner tool call goes through the same
/// dispatch path as `call_tool`, preserving audit and visibility filtering.
pub struct ComposeTool {
    sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
    top_level: Arc<RwLock<HashMap<String, Arc<dyn TopLevelTool>>>>,
}

impl ComposeTool {
    pub fn new(
        sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
        top_level: Arc<RwLock<HashMap<String, Arc<dyn TopLevelTool>>>>,
    ) -> Self {
        Self { sets, top_level }
    }
}

static COMPOSE_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<ComposeParams>);

// ---------------------------------------------------------------------------
// Output shape (schemars-derived, also used for serialization)
// ---------------------------------------------------------------------------

#[derive(serde::Serialize, schemars::JsonSchema)]
struct ComposeOutput {
    /// The value returned by the script.
    result: serde_json::Value,
    /// Console output captured during execution.
    console: Vec<String>,
    /// Number of tool calls made.
    tool_calls: usize,
    /// Execution time in milliseconds.
    execution_time_ms: u64,
}

static COMPOSE_OUTPUT_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<ComposeOutput>);

#[async_trait::async_trait]
impl TopLevelTool for ComposeTool {
    fn name(&self) -> &str {
        "compose"
    }

    fn description(&self) -> &str {
        "Execute JavaScript that composes multiple tool calls in a single round trip. \
         The script has access to a `tools` namespace with nested server namespaces \
         (e.g. `tools.honeycomb.list_environments({...})`). Flat prefixed names also work \
         (e.g. `tools.honeycomb_list_environments({...})`). \
         Use `return` for the final value. Top-level `await` and `Promise.all()` are supported. \
         TypeScript declarations for available tools are included in the execution context.\n\n\
         Example:\n```js\nconst envs = await tools.honeycomb.list_environments({});\n\
         const issues = await tools.github.list_issues({ repo: 'org/repo', state: 'open' });\n\
         const stale = issues.filter(i => Date.now() - Date.parse(i.updated_at) > 7*86400*1000);\n\
         return { envs, stale_issues: stale.map(i => i.number) };\n```"
    }

    fn input_schema(&self) -> &serde_json::Value {
        &COMPOSE_SCHEMA
    }

    fn output_schema(&self) -> Option<&serde_json::Value> {
        Some(&COMPOSE_OUTPUT_SCHEMA)
    }

    fn composable(&self) -> bool {
        false
    }

    #[tracing::instrument(name = "toolset.compose.call", skip_all)]
    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: ComposeParams = parse_params(arguments)?;

        let timeout = match params.timeout_ms {
            Some(ms) if ms > 0 => Duration::from_millis(ms as u64).min(MAX_TIMEOUT),
            _ => DEFAULT_TIMEOUT,
        };

        Audit::record_action("compose".to_string());

        // Generate TypeScript declarations from the visible catalog + top-level tools
        let dts = {
            let sets = self.sets.read().expect("toolset lock poisoned");
            let top = self.top_level.read().expect("top_level lock poisoned");
            generate_dts(subject, &sets, &top)
        };

        // Prepend type declarations as a JS block comment so the script
        // context is self-documenting (helpful for error diagnostics).
        let script = if dts.is_empty() {
            params.script
        } else {
            format!("/*\n{dts}*/\n{}", params.script)
        };

        let dispatcher = Arc::new(CatalogDispatcher {
            sets: Arc::clone(&self.sets),
            top_level: Arc::clone(&self.top_level),
            subject: subject.clone(),
        });

        let engine = js_engine::JsEngine::new();
        let result = engine
            .execute(&script, dispatcher, timeout)
            .await
            .map_err(|e| ToolSetsError::Compose(e.to_string()))?;

        // Format the output
        let mut sections = Vec::new();

        // Main result
        let value_str =
            serde_json::to_string_pretty(&result.value).unwrap_or_else(|_| "null".to_string());
        sections.push(format!("=== Result ===\n{value_str}"));

        // Console output (if any)
        if !result.console_output.is_empty() {
            sections.push(format!(
                "=== Console ===\n{}",
                result.console_output.join("\n")
            ));
        }

        // Available namespaces summary
        if !dts.is_empty() {
            sections.push(format!("=== Available Types ===\n{dts}"));
        }

        // Metadata
        sections.push(format!(
            "=== Metadata ===\ntool_calls: {}\nexecution_time: {:?}",
            result.tool_calls_made, result.execution_time
        ));

        let text = sections.join("\n\n");
        let out = ComposeOutput {
            result: result.value,
            console: result.console_output,
            tool_calls: result.tool_calls_made,
            execution_time_ms: result.execution_time.as_millis() as u64,
        };
        let structured = serde_json::to_value(&out).expect("ComposeOutput serialization");
        let mut ctr = CallToolResult::success(vec![Content::text(text)]);
        ctr.structured_content = Some(structured);
        Ok(ctr)
    }
}

// ─── Catalog Dispatcher ──────────────────────────────────────────────────────

/// Bridges the JS engine's [`js_engine::ToolDispatcher`] trait to both the
/// catalog (SearchableToolSet) dispatch path and the top-level tool registry,
/// preserving visibility filtering, audit logging, and output filtering.
struct CatalogDispatcher {
    sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
    top_level: Arc<RwLock<HashMap<String, Arc<dyn TopLevelTool>>>>,
    subject: AuthSubject,
}

#[async_trait::async_trait]
impl js_engine::ToolDispatcher for CatalogDispatcher {
    async fn call_tool(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        // First: try catalog sets (prefixed names like "honeycomb_list_environments")
        if let Ok((set, tool_name, default_filter)) = self.find_set(name) {
            Audit::record_action(format!("compose > catalog: {name}"));

            let inner_args = match args {
                serde_json::Value::Object(obj) => Some(obj),
                serde_json::Value::Null => None,
                _ => return Err(format!("Expected object arguments, got: {args}")),
            };

            let result = set
                .call(&self.subject, &tool_name, inner_args)
                .await
                .map_err(|e| e.to_string())?;

            let filter = default_filter.unwrap_or_else(OutputFilter::global_default);
            let filtered = filter.apply(result).map_err(|e| e.to_string())?;

            if let Some(structured) = &filtered.structured_content {
                return Ok(structured.clone());
            }

            let text = extract_text(&filtered);
            return match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(v) => Ok(v),
                Err(_) => Ok(serde_json::Value::String(text)),
            };
        }

        // Fall back: top-level tool (e.g. "bash", "read", "glob")
        self.call_top_level(name, args).await
    }
}

impl CatalogDispatcher {
    /// Locate the `SearchableToolSet` backing `prefixed_name`, filtered by
    /// the subject's visibility. Returns the toolset, inner tool name, and
    /// default output filter. Same logic as `CallCatalogTool::find_set()`.
    #[allow(clippy::type_complexity)]
    fn find_set(
        &self,
        prefixed_name: &str,
    ) -> Result<(Arc<dyn SearchableToolSet>, String, Option<OutputFilter>), ToolSetsError> {
        let sets = self.sets.read().expect("toolset lock poisoned");
        for set in sets.iter() {
            if !set.is_visible(&self.subject) {
                continue;
            }
            let prefix = format!("{}_", set.prefix());
            if let Some(tool_name) = prefixed_name.strip_prefix(&prefix) {
                if let Some(entry) = set.tools().iter().find(|t| t.name == tool_name) {
                    return Ok((
                        Arc::clone(set),
                        tool_name.to_string(),
                        entry.default_output_filter.clone(),
                    ));
                }
            }
        }
        Err(ToolSetsError::ToolNotFound(prefixed_name.to_string()))
    }

    /// Dispatch to a top-level tool by exact name. Respects visibility and
    /// each tool's [`TopLevelTool::composable`] flag.
    async fn call_top_level(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let tool = {
            let map = self.top_level.read().expect("top_level lock poisoned");
            map.get(name)
                .filter(|t| t.composable() && t.is_visible(&self.subject))
                .cloned()
                .ok_or_else(|| format!("Tool not found: {name}"))?
        };

        Audit::record_action(format!("compose > top_level: {name}"));

        let inner_args = match args {
            serde_json::Value::Object(obj) => Some(obj),
            serde_json::Value::Null => None,
            _ => return Err(format!("Expected object arguments, got: {args}")),
        };

        let result = tool
            .call(&self.subject, inner_args)
            .await
            .map_err(|e| e.to_string())?;

        // Prefer structured_content (typed JSON) over text parsing
        if let Some(structured) = &result.structured_content {
            return Ok(structured.clone());
        }

        let text = extract_text(&result);
        match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) => Ok(v),
            Err(_) => Ok(serde_json::Value::String(text)),
        }
    }
}

/// Extract all text content from a [`CallToolResult`] into a single string.
fn extract_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ─── TypeScript Declaration Generation ───────────────────────────────────────

/// Generate TypeScript declarations from the visible catalog entries and
/// top-level tools, grouped by server prefix. Output looks like:
///
/// ```ts
/// declare namespace tools {
///   function bash(args: { command: string; ... }): Promise<any>;
///   function read(args: { file_path: string; ... }): Promise<any>;
///   namespace honeycomb {
///     function list_environments(args: { ... }): Promise<{ env_id: string }>;
///   }
/// }
/// ```
///
/// When a tool has an `output_schema`, the return type is derived from that
/// schema instead of the default `any`.
pub(super) fn generate_dts(
    subject: &AuthSubject,
    sets: &[Arc<dyn SearchableToolSet>],
    top_level: &HashMap<String, Arc<dyn TopLevelTool>>,
) -> String {
    // Top-level tools (flat, no namespace prefix)
    let mut top_fns: Vec<(String, String, String)> = Vec::new();
    for (name, tool) in top_level.iter() {
        if !tool.composable() || !tool.is_visible(subject) {
            continue;
        }
        let params_ts = schema_to_ts_params(tool.input_schema());
        let return_ts = tool
            .output_schema()
            .map(output_schema_to_ts)
            .unwrap_or_else(|| "any".to_string());
        top_fns.push((name.clone(), params_ts, return_ts));
    }
    top_fns.sort_by(|a, b| a.0.cmp(&b.0));

    // Catalog tools grouped by server prefix: (tool_name, params_ts, return_ts)
    let mut namespaces: BTreeMap<String, Vec<(String, String, String)>> = BTreeMap::new();
    for set in sets.iter() {
        if !set.is_visible(subject) {
            continue;
        }
        let prefix = set.prefix().to_string();
        let tools_in_ns = namespaces.entry(prefix).or_default();
        for entry in set.tools() {
            let schema_val =
                serde_json::Value::Object(entry.description.input_schema.as_ref().clone());
            let params_ts = schema_to_ts_params(&schema_val);
            let return_ts = entry
                .description
                .output_schema
                .as_ref()
                .map(|s| {
                    let schema_val = serde_json::Value::Object(s.as_ref().clone());
                    output_schema_to_ts(&schema_val)
                })
                .unwrap_or_else(|| "any".to_string());
            tools_in_ns.push((entry.name.clone(), params_ts, return_ts));
        }
    }

    if namespaces.is_empty() && top_fns.is_empty() {
        return String::new();
    }

    let mut lines = vec!["declare namespace tools {".to_string()];

    // Top-level functions first
    for (name, params, ret) in &top_fns {
        lines.push(format!(
            "  function {name}(args: {{ {params} }}): Promise<{ret}>;"
        ));
    }

    // Then namespace-grouped catalog tools
    for (ns, tools) in &namespaces {
        lines.push(format!("  namespace {ns} {{"));
        for (name, params, ret) in tools {
            lines.push(format!(
                "    function {name}(args: {{ {params} }}): Promise<{ret}>;"
            ));
        }
        lines.push("  }".to_string());
    }
    lines.push("}".to_string());
    lines.join("\n")
}

/// Convert a JSON Schema `input_schema` object into a TypeScript parameter
/// string. Handles common JSON Schema types; nested objects become inline
/// object types. Arrays become `T[]`.
///
/// Example output: `repo: string; state?: string; limit?: number`
pub(super) fn schema_to_ts_params(schema: &serde_json::Value) -> String {
    let properties = match schema.get("properties").and_then(|p| p.as_object()) {
        Some(p) => p,
        None => return "...args: any".to_string(),
    };

    let required: Vec<&str> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let mut parts = Vec::new();
    for (name, prop_schema) in properties {
        let ts_type = json_schema_to_ts(prop_schema);
        let optional = if required.contains(&name.as_str()) {
            ""
        } else {
            "?"
        };
        parts.push(format!("{name}{optional}: {ts_type}"));
    }
    parts.join("; ")
}

/// Convert a single JSON Schema type definition to a TypeScript type string.
pub(super) fn json_schema_to_ts(schema: &serde_json::Value) -> &'static str {
    match schema.get("type").and_then(|t| t.as_str()) {
        Some("string") => "string",
        Some("number" | "integer") => "number",
        Some("boolean") => "boolean",
        Some("array") => "any[]",
        Some("object") => "Record<string, any>",
        Some("null") => "null",
        _ => "any",
    }
}

/// Convert an output JSON Schema (root type "object") into a TypeScript
/// inline object type, e.g. `{ temperature: number; humidity: number }`.
/// Falls back to `any` for schemas that aren't simple object types.
pub(super) fn output_schema_to_ts(schema: &serde_json::Value) -> String {
    let properties = match schema.get("properties").and_then(|p| p.as_object()) {
        Some(p) if !p.is_empty() => p,
        _ => return "any".to_string(),
    };

    let required: Vec<&str> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let mut parts = Vec::new();
    for (name, prop_schema) in properties {
        let ts_type = json_schema_to_ts(prop_schema);
        let optional = if required.contains(&name.as_str()) {
            ""
        } else {
            "?"
        };
        parts.push(format!("{name}{optional}: {ts_type}"));
    }
    format!("{{ {} }}", parts.join("; "))
}
