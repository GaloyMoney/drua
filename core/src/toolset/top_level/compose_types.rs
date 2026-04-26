//! `compose_types` — return TypeScript declarations for specific tools, for
//! use with `compose`. A batched, code-focused alternative to `describe_tool`
//! that gives agents typed function signatures before writing compose scripts.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, RwLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use serde::Deserialize;

use crate::auth::AuthSubject;

use super::super::error::ToolSetsError;
use super::super::traits::{SearchableToolSet, TopLevelTool};
use super::compose::{output_schema_to_ts, schema_to_ts_params};
use super::{parse_params, schema_for};

// ---------------------------------------------------------------------------
// Params
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
struct ComposeTypesParams {
    /// Prefixed tool names to generate declarations for (e.g.
    /// `["honeycomb_list_environments", "github_list_issues"]`).
    /// Use `"*"` as a single element to get all visible tools.
    tool_names: Vec<String>,
}

// ---------------------------------------------------------------------------
// Tool
// ---------------------------------------------------------------------------

pub struct ComposeTypes {
    sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
}

impl ComposeTypes {
    pub fn new(sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>) -> Self {
        Self { sets }
    }
}

static COMPOSE_TYPES_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<ComposeTypesParams>);

static COMPOSE_TYPES_OUTPUT_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "declarations": {
                "type": "string",
                "description": "TypeScript declarations for the requested tools"
            },
            "tool_count": {
                "type": "integer",
                "description": "Number of tools included in the declarations"
            },
            "not_found": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Tool names that were not found"
            }
        },
        "required": ["declarations", "tool_count", "not_found"]
    })
});

#[async_trait::async_trait]
impl TopLevelTool for ComposeTypes {
    fn name(&self) -> &str {
        "compose_types"
    }

    fn description(&self) -> &str {
        "Get TypeScript declarations for specific tools, for use with compose. \
         Returns typed function signatures with input parameters and output types."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &COMPOSE_TYPES_SCHEMA
    }

    fn output_schema(&self) -> Option<&serde_json::Value> {
        Some(&COMPOSE_TYPES_OUTPUT_SCHEMA)
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: ComposeTypesParams = parse_params(arguments)?;

        let sets = self.sets.read().expect("toolset lock poisoned");

        let want_all = params.tool_names.len() == 1 && params.tool_names[0] == "*";

        // Build a (prefix, tool_name, params_ts, return_ts) list for matching tools
        let mut namespaces: BTreeMap<String, Vec<(String, String, String)>> = BTreeMap::new();
        let mut matched: Vec<String> = Vec::new();

        for set in sets.iter() {
            if !set.is_visible(subject) {
                continue;
            }
            let prefix = set.prefix().to_string();
            for entry in set.tools() {
                let prefixed_name = format!("{}_{}", prefix, entry.name);
                if !want_all && !params.tool_names.contains(&prefixed_name) {
                    continue;
                }

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

                namespaces.entry(prefix.clone()).or_default().push((
                    entry.name.clone(),
                    params_ts,
                    return_ts,
                ));
                matched.push(prefixed_name);
            }
        }

        let not_found: Vec<String> = if want_all {
            Vec::new()
        } else {
            params
                .tool_names
                .iter()
                .filter(|name| !matched.contains(name))
                .cloned()
                .collect()
        };

        // Format as .d.ts
        let dts = if namespaces.is_empty() {
            "// No matching tools found".to_string()
        } else {
            let mut lines = vec!["declare namespace tools {".to_string()];
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
        };

        let tool_count = matched.len();

        let mut text = dts.clone();
        if !not_found.is_empty() {
            text.push_str(&format!("\n\n// Not found: {}", not_found.join(", ")));
        }

        let structured = serde_json::json!({
            "declarations": dts,
            "tool_count": tool_count,
            "not_found": not_found,
        });
        let mut result = CallToolResult::success(vec![Content::text(text)]);
        result.structured_content = Some(structured);
        Ok(result)
    }
}
