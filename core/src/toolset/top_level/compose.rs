//! `compose` — execute JavaScript that chains multiple MCP tool calls in a
//! single round trip. The script has access to a `tools` proxy; each
//! `tools.prefixed_name(args)` call dispatches to the backing catalog toolset
//! and is individually audit-logged.
//!
//! Includes TypeScript declaration generation from catalog JSON Schemas so
//! agents see typed APIs in the execution context.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, RwLock};
use std::time::Duration;

use rmcp::model::{CallToolResult, Content, JsonObject};

use crate::audit::Audit;
use crate::auth::AuthSubject;

use super::super::error::ToolSetsError;
use super::super::filter::OutputFilter;
use super::super::traits::{SearchableToolSet, TopLevelTool};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// A `TopLevelTool` that evaluates JavaScript with access to upstream MCP
/// tools via a `tools.*` proxy. Each inner tool call goes through the same
/// dispatch path as `call_tool`, preserving audit and visibility filtering.
pub struct ComposeTool {
    sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
}

impl ComposeTool {
    pub fn new(sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>) -> Self {
        Self { sets }
    }
}

static COMPOSE_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "script": {
                "type": "string",
                "description": "JavaScript code to execute. Has access to `tools.*` proxy for calling upstream MCP tools. Use `return` for the final value. Top-level `await` is supported."
            }
        },
        "required": ["script"],
        "additionalProperties": false
    })
});

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

    #[tracing::instrument(name = "toolset.compose.call", skip_all)]
    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let script = arguments
            .as_ref()
            .and_then(|a| a.get("script"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolSetsError::MissingArgument("script".to_string()))?
            .to_string();

        Audit::record_action("compose".to_string());

        // Generate TypeScript declarations from the visible catalog
        let dts = {
            let sets = self.sets.read().expect("toolset lock poisoned");
            generate_dts(subject, &sets)
        };

        // Prepend type declarations as a JS block comment so the script
        // context is self-documenting (helpful for error diagnostics).
        let script_with_types = if dts.is_empty() {
            script
        } else {
            format!("/*\n{dts}*/\n{script}")
        };

        let dispatcher = Arc::new(CatalogDispatcher {
            sets: Arc::clone(&self.sets),
            subject: subject.clone(),
        });

        let engine = js_engine::JsEngine::new();
        let result = engine
            .execute(&script_with_types, dispatcher, DEFAULT_TIMEOUT)
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
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}

// ─── Catalog Dispatcher ──────────────────────────────────────────────────────

/// Bridges the JS engine's [`js_engine::ToolDispatcher`] trait to the catalog
/// toolset dispatch path, preserving visibility filtering, audit logging, and
/// output filtering.
struct CatalogDispatcher {
    sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
    subject: AuthSubject,
}

#[async_trait::async_trait]
impl js_engine::ToolDispatcher for CatalogDispatcher {
    async fn call_tool(
        &self,
        prefixed_name: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let (set, tool_name, default_filter) =
            self.find_set(prefixed_name).map_err(|e| e.to_string())?;

        Audit::record_action(format!("compose > catalog: {prefixed_name}"));

        let inner_args = match args {
            serde_json::Value::Object(obj) => Some(obj),
            serde_json::Value::Null => None,
            _ => return Err(format!("Expected object arguments, got: {args}")),
        };

        let result = set
            .call(&self.subject, &tool_name, inner_args)
            .await
            .map_err(|e| e.to_string())?;

        // Apply default output filter
        let filter = default_filter.unwrap_or_else(OutputFilter::global_default);
        let filtered = filter.apply(result).map_err(|e| e.to_string())?;

        // Extract text content as the return value
        let text = extract_text(&filtered);
        // Try to parse as JSON; if it fails, return as a string value
        match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) => Ok(v),
            Err(_) => Ok(serde_json::Value::String(text)),
        }
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

/// Generate TypeScript declarations from the visible catalog entries,
/// grouped by server prefix. Output looks like:
///
/// ```ts
/// declare namespace tools {
///   namespace honeycomb {
///     function list_environments(args: { ... }): Promise<any>;
///   }
/// }
/// ```
fn generate_dts(subject: &AuthSubject, sets: &[Arc<dyn SearchableToolSet>]) -> String {
    // Group tools by server prefix
    let mut namespaces: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();

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
            tools_in_ns.push((entry.name.clone(), params_ts));
        }
    }

    if namespaces.is_empty() {
        return String::new();
    }

    let mut lines = vec!["declare namespace tools {".to_string()];
    for (ns, tools) in &namespaces {
        lines.push(format!("  namespace {ns} {{"));
        for (name, params) in tools {
            lines.push(format!(
                "    function {name}(args: {{ {params} }}): Promise<any>;"
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
fn schema_to_ts_params(schema: &serde_json::Value) -> String {
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
fn json_schema_to_ts(schema: &serde_json::Value) -> &'static str {
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
