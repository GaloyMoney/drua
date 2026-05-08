//! `compose_types` — return TypeScript declarations for specific tools, for
//! use with `compose`. A batched, code-focused alternative to `describe_tool`
//! that gives agents typed function signatures before writing compose scripts.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, RwLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use serde::Deserialize;

use crate::auth::AuthSubject;

use super::super::error::ToolSetsError;
use super::super::traits::{SearchableToolSet, TopLevelTool};
use super::{parse_params, schema_for};

#[derive(Deserialize, schemars::JsonSchema)]
struct ComposeTypesParams {
    /// Prefixed tool names. Each entry may be an exact name (`concourse_get_build_logs`)
    /// or a single-trailing-`*` prefix glob (`concourse_*`, `concourse_get_*`).
    /// Use `"*"` as the only element for every visible tool.
    tool_names: Vec<String>,
}

/// `"concourse_*"` → matches any name starting with `"concourse_"`.
/// `"concourse_get_logs"` → exact match.
/// Bare `"*"` is handled separately as the "all visible tools" sentinel.
fn matches_pattern(name: &str, pattern: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        name.starts_with(prefix)
    } else {
        name == pattern
    }
}

fn any_match(name: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| matches_pattern(name, p))
}

pub struct ComposeTypes {
    sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
    top_level: Arc<RwLock<HashMap<String, Arc<dyn TopLevelTool>>>>,
}

impl ComposeTypes {
    pub fn new(
        sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
        top_level: Arc<RwLock<HashMap<String, Arc<dyn TopLevelTool>>>>,
    ) -> Self {
        Self { sets, top_level }
    }
}

static COMPOSE_TYPES_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<ComposeTypesParams>);

#[derive(serde::Serialize, schemars::JsonSchema)]
struct ComposeTypesOutput {
    declarations: String,
    tool_count: usize,
    not_found: Vec<String>,
}

static COMPOSE_TYPES_OUTPUT_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<ComposeTypesOutput>);

#[async_trait::async_trait]
impl TopLevelTool for ComposeTypes {
    fn name(&self) -> &str {
        "compose_types"
    }

    fn description(&self) -> &str {
        "Get TypeScript declarations for specific tools, for use with compose. \
         Returns typed function signatures with input parameters and output types. \
         `tool_names` accepts exact names (`concourse_get_build_logs`), \
         single-trailing-`*` prefix globs (`concourse_*`, `concourse_get_*`), \
         and bare `\"*\"` (alone) for every visible tool. \
         Tools with `Promise<any>` return type have no declared output schema — \
         their result may be string or JSON depending on the tool."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &COMPOSE_TYPES_SCHEMA
    }

    fn output_schema(&self) -> Option<&serde_json::Value> {
        Some(&COMPOSE_TYPES_OUTPUT_SCHEMA)
    }

    fn composable(&self) -> bool {
        false
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: ComposeTypesParams = parse_params(arguments)?;

        let sets = self.sets.read().expect("toolset lock poisoned");
        let top = self.top_level.read().expect("top_level lock poisoned");

        let want_all = params.tool_names.len() == 1 && params.tool_names[0] == "*";

        let mut top_fns: Vec<(String, String, String)> = Vec::new();
        let mut matched: Vec<String> = Vec::new();

        for (name, tool) in top.iter() {
            if !tool.composable() || !tool.is_visible(subject) {
                continue;
            }
            if !want_all && !any_match(name, &params.tool_names) {
                continue;
            }
            let params_ts = json_schema_ts::schema_to_ts_params(tool.input_schema());
            let return_ts = tool
                .output_schema()
                .map(json_schema_ts::schema_to_ts)
                .unwrap_or_else(|| "any".to_string());
            top_fns.push((name.clone(), params_ts, return_ts));
            matched.push(name.clone());
        }
        top_fns.sort_by(|a, b| a.0.cmp(&b.0));

        let mut namespaces: BTreeMap<String, Vec<(String, String, String)>> = BTreeMap::new();

        for set in sets.iter() {
            if !set.is_visible(subject) {
                continue;
            }
            let prefix = set.prefix().to_string();
            for entry in set.tools() {
                let prefixed_name = format!("{}_{}", prefix, entry.name);
                if !want_all && !any_match(&prefixed_name, &params.tool_names) {
                    continue;
                }

                let schema_val =
                    serde_json::Value::Object(entry.description.input_schema.as_ref().clone());
                let params_ts = json_schema_ts::schema_to_ts_params(&schema_val);
                let return_ts = entry
                    .description
                    .output_schema
                    .as_ref()
                    .map(|s| {
                        let schema_val = serde_json::Value::Object(s.as_ref().clone());
                        json_schema_ts::schema_to_ts(&schema_val)
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

        // A glob pattern is "found" if it matched at least one tool.
        let not_found: Vec<String> = if want_all {
            Vec::new()
        } else {
            params
                .tool_names
                .iter()
                .filter(|pat| !matched.iter().any(|m| matches_pattern(m, pat)))
                .cloned()
                .collect()
        };

        let has_content = !top_fns.is_empty() || !namespaces.is_empty();
        let dts = if !has_content {
            "// No matching tools found".to_string()
        } else {
            let mut lines = vec!["declare namespace tools {".to_string()];

            for (name, params, ret) in &top_fns {
                lines.push(format!(
                    "  function {name}(args: {{ {params} }}): Promise<{ret}>;"
                ));
            }

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

        let out = ComposeTypesOutput {
            declarations: dts,
            tool_count,
            not_found,
        };
        let structured = serde_json::to_value(&out).expect("ComposeTypesOutput serialization");
        let mut result = CallToolResult::success(vec![Content::text(text)]);
        result.structured_content = Some(structured);
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> String {
        s.to_string()
    }

    #[test]
    fn exact_name_matches() {
        assert!(matches_pattern(
            "concourse_get_build_logs",
            "concourse_get_build_logs"
        ));
        assert!(!matches_pattern(
            "concourse_get_build_logs",
            "concourse_get_build_log"
        ));
    }

    #[test]
    fn trailing_star_is_prefix_glob() {
        assert!(matches_pattern("concourse_get_build_logs", "concourse_*"));
        assert!(matches_pattern("concourse_list_jobs", "concourse_*"));
        assert!(!matches_pattern("github_list_prs", "concourse_*"));

        assert!(matches_pattern(
            "concourse_get_build_logs",
            "concourse_get_*"
        ));
        assert!(!matches_pattern("concourse_list_jobs", "concourse_get_*"));
    }

    #[test]
    fn bare_star_matches_anything_under_any_match() {
        assert!(any_match("concourse_get_build_logs", &[p("*")]));
        assert!(any_match("anything_at_all", &[p("*")]));
    }

    #[test]
    fn any_match_unions_patterns() {
        let patterns = vec![p("github_list_*"), p("concourse_get_build_logs")];
        assert!(any_match("github_list_prs", &patterns));
        assert!(any_match("concourse_get_build_logs", &patterns));
        assert!(!any_match("concourse_list_jobs", &patterns));
    }
}
