//! Catalog-backed meta-tools: `search_tools`, `describe_tool`, and
//! `call_tool`. The first two are read-only; `call_tool` dispatches into
//! an upstream `SearchableToolSet`.
//!
//! Each tool struct owns its read-locked registry access and its
//! result-formatting helpers. Anything genuinely shared across more than
//! one tool (visibility-filtered catalog enumeration, string helpers used
//! by scoring / brief descriptions) lives in the "shared helpers" block
//! at the bottom of the file.

use std::sync::{Arc, LazyLock, RwLock};

use drua_tool_caching::ToolCaching;
use serde_json::json;

use rmcp::model::{CallToolResult, Content, JsonObject, Tool};

use crate::audit::Audit;
use crate::auth::AuthSubject;

use super::super::error::ToolSetsError;
use super::super::traits::{SearchableToolSet, TopLevelTool};
use super::super::wrap::wrap_output_schema;
use super::schema_for;

pub struct CatalogEntry {
    pub prefixed_name: String,
    pub upstream_name: String,
    pub tool_name: String,
    pub category: String,
    pub brief_description: String,
    pub full_tool: Tool,
}

pub struct SearchCatalog {
    sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
}

impl SearchCatalog {
    pub fn new(sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>) -> Self {
        Self { sets }
    }

    fn execute_search(
        &self,
        subject: &AuthSubject,
        query: Option<&str>,
        category: Option<&str>,
    ) -> Vec<CatalogEntry> {
        let sets = self.sets.read().expect("toolset lock poisoned");
        let mut entries: Vec<CatalogEntry> = visible_entries(subject, &sets)
            .into_iter()
            .filter(|e| {
                if let Some(cat) = category {
                    if cat != "all" && e.category != cat {
                        return false;
                    }
                }
                true
            })
            .collect();

        // Wildcard / empty query → return all (category-filtered) entries.
        let query = query.filter(|q| {
            let t = q.trim();
            !(t.is_empty() || t == "*")
        });

        if let Some(q) = query {
            let keywords: Vec<String> = Self::normalize(q)
                .split_whitespace()
                .map(String::from)
                .collect();
            if !keywords.is_empty() {
                let mut scored: Vec<_> = entries
                    .into_iter()
                    .filter_map(|e| {
                        let score = Self::keyword_score(&e, &keywords);
                        if score > 0 {
                            Some((score, e))
                        } else {
                            None
                        }
                    })
                    .collect();
                scored.sort_by_key(|x| std::cmp::Reverse(x.0));
                entries = scored.into_iter().map(|(_, e)| e).collect();
            }
        }

        entries
    }

    /// Lowercase and collapse `_`/`-` into spaces so "search code",
    /// "search_code", and "search-code" all match each other.
    fn normalize(s: &str) -> String {
        s.to_lowercase().replace(['_', '-'], " ")
    }

    fn keyword_score(entry: &CatalogEntry, keywords: &[String]) -> usize {
        let haystack = [
            Self::normalize(&entry.tool_name),
            Self::normalize(&entry.upstream_name),
            Self::normalize(&entry.brief_description),
        ]
        .join(" ");
        keywords
            .iter()
            .filter(|kw| haystack.contains(kw.as_str()))
            .count()
    }

    fn format_results(results: &[CatalogEntry]) -> String {
        if results.is_empty() {
            return "No tools found matching your query.".to_string();
        }
        let mut lines = Vec::new();
        let mut current_category: Option<&str> = None;
        for entry in results {
            let cat = entry.category.as_str();
            if current_category != Some(cat) {
                if !lines.is_empty() {
                    lines.push(String::new());
                }
                lines.push(format!("{cat}:"));
                current_category = Some(cat);
            }
            lines.push(format!(
                "  {:40} - {}",
                entry.prefixed_name, entry.brief_description
            ));
        }
        lines.join("\n")
    }
}

static SEARCH_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "Free-form search query" },
            "category": { "type": "string", "description": "Optional category filter ('all' for any)" }
        }
    })
});

#[derive(serde::Serialize, schemars::JsonSchema)]
struct SearchToolsOutput {
    tools: Vec<SearchToolEntry>,
    total: usize,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct SearchToolEntry {
    name: String,
    category: String,
    description: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct DescribeToolOutput {
    name: String,
    upstream: String,
    category: String,
    description: String,
    #[schemars(schema_with = "crate::toolset::any_json_schema")]
    input_schema: serde_json::Value,
    // `#[schemars(default)]` is required, not redundant with `Option`: when
    // `schema_with` overrides a field's type schema, schemars 0.8 loses track
    // of the `Option` and marks the field `required`. Strict MCP clients
    // (pi ships @modelcontextprotocol/sdk 1.29.0) validate `structuredContent`
    // against this schema and reject responses missing a `required` field —
    // which is why `describe_tool` failed for every tool that has no upstream
    // `output_schema`. `schemars(default)` restores the not-required semantics
    // while `schema_with` keeps the field an any-schema object (not a boolean
    // schema, which strict clients also reject).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(default)]
    #[schemars(schema_with = "crate::toolset::any_json_schema")]
    output_schema: Option<serde_json::Value>,
}

static SEARCH_OUTPUT_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<SearchToolsOutput>);

#[async_trait::async_trait]
impl TopLevelTool for SearchCatalog {
    fn name(&self) -> &str {
        "search_tools"
    }
    fn description(&self) -> &str {
        "Search for available tools across all upstream services. Returns tool \
         names, brief descriptions, and categories. Use this first to find \
         relevant tools before calling them."
    }
    fn input_schema(&self) -> &serde_json::Value {
        &SEARCH_SCHEMA
    }
    fn inner_output_schema(&self) -> Option<&serde_json::Value> {
        Some(&SEARCH_OUTPUT_SCHEMA)
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        Audit::record_action("search_tools");
        let args = arguments.as_ref();
        let query = args.and_then(|a| a.get("query")).and_then(|v| v.as_str());
        let category = args
            .and_then(|a| a.get("category"))
            .and_then(|v| v.as_str());
        let results = self.execute_search(subject, query, category);
        let text = if results.is_empty() {
            format!(
                "No tools found matching {:?}.\n\n\
                 search_tools matches by tool name, category, and description. \
                 Try keywords describing the *action* you want \
                 (e.g. `build`, `concourse`, `github`, `k8s`, `postgres`), \
                 not project or repo names.",
                query.unwrap_or(""),
            )
        } else {
            Self::format_results(&results)
        };
        let out = SearchToolsOutput {
            total: results.len(),
            tools: results
                .iter()
                .map(|e| SearchToolEntry {
                    name: e.prefixed_name.clone(),
                    category: e.category.clone(),
                    description: e.brief_description.clone(),
                })
                .collect(),
        };
        let structured = serde_json::to_value(&out).expect("SearchToolsOutput serialization");
        let mut result = CallToolResult::success(vec![Content::text(text)]);
        result.structured_content = Some(structured);
        Ok(result)
    }
}

pub struct DescribeCatalogTool {
    sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
}

impl DescribeCatalogTool {
    pub fn new(sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>) -> Self {
        Self { sets }
    }

    fn execute_describe(&self, subject: &AuthSubject, prefixed_name: &str) -> Option<CatalogEntry> {
        let sets = self.sets.read().expect("toolset lock poisoned");
        visible_entries(subject, &sets)
            .into_iter()
            .find(|e| e.prefixed_name == prefixed_name)
    }

    fn format_entry(entry: &CatalogEntry) -> String {
        let tool = &entry.full_tool;
        let description = tool
            .description
            .as_deref()
            .unwrap_or("No description available.");
        let schema =
            serde_json::to_string_pretty(&tool.input_schema).unwrap_or_else(|_| "{}".into());
        let wrapped_output_schema = tool
            .output_schema
            .as_ref()
            .map(|s| wrap_output_schema(&serde_json::Value::Object(s.as_ref().clone())));
        let output_section = wrapped_output_schema
            .as_ref()
            .and_then(|s| serde_json::to_string_pretty(s).ok())
            .map(|s| format!("\n\n### Output schema (drua-wrapped)\n```json\n{s}\n```"))
            .unwrap_or_default();

        // Embed a TS signature so agents writing `compose` scripts can read the typed
        // shape inline; `compose_types` remains useful for batch/prefix-glob lookups.
        let input_schema_value = serde_json::Value::Object(tool.input_schema.as_ref().clone());
        let input_ts = json_schema_ts::schema_to_ts_params(&input_schema_value);
        let inner_ts = tool
            .output_schema
            .as_ref()
            .map(|s| {
                let v = serde_json::Value::Object(s.as_ref().clone());
                json_schema_ts::schema_to_ts(&v)
            })
            .unwrap_or_else(|| "any".to_string());
        let ts_signature = format!(
            "\n\n### TypeScript signature (for use in `compose`)\n```ts\nfunction {tool}(args: {{ {input_ts} }}): Promise<{inner_ts}>;\n```",
            tool = entry.tool_name,
        );

        format!(
            "## {}\n\nUpstream: {}\nCategory: {}\n\n{}\n\n### Parameters\n```json\n{}\n```{}{}\n\nUse call_tool(\"{}\", {{...}}) to execute.",
            entry.prefixed_name,
            entry.upstream_name,
            entry.category,
            description,
            schema,
            output_section,
            ts_signature,
            entry.prefixed_name,
        )
    }
}

static DESCRIBE_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "tool_name": { "type": "string", "description": "The prefixed tool name returned from search_tools" }
        },
        "required": ["tool_name"]
    })
});

static DESCRIBE_OUTPUT_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<DescribeToolOutput>);

#[async_trait::async_trait]
impl TopLevelTool for DescribeCatalogTool {
    fn name(&self) -> &str {
        "describe_tool"
    }
    fn description(&self) -> &str {
        "Get the full parameter schema and detailed description for a specific \
         tool. Use after search_tools to understand how to call a tool."
    }
    fn input_schema(&self) -> &serde_json::Value {
        &DESCRIBE_SCHEMA
    }
    fn inner_output_schema(&self) -> Option<&serde_json::Value> {
        Some(&DESCRIBE_OUTPUT_SCHEMA)
    }
    fn default_tool_caching(&self) -> bool {
        false
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let tool_name = arguments
            .as_ref()
            .and_then(|a| a.get("tool_name"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        match self.execute_describe(subject, tool_name) {
            Some(entry) => {
                let text = Self::format_entry(&entry);
                let tool = &entry.full_tool;
                let out = DescribeToolOutput {
                    name: entry.prefixed_name,
                    upstream: entry.upstream_name,
                    category: entry.category,
                    description: tool.description.as_deref().unwrap_or("").to_string(),
                    input_schema: serde_json::Value::Object(tool.input_schema.as_ref().clone()),
                    output_schema: tool.output_schema.as_ref().map(|s| {
                        wrap_output_schema(&serde_json::Value::Object(s.as_ref().clone()))
                    }),
                };
                let structured =
                    serde_json::to_value(&out).expect("DescribeToolOutput serialization");
                let mut result = CallToolResult::success(vec![Content::text(text)]);
                result.structured_content = Some(structured);
                Ok(result)
            }
            None => Ok(CallToolResult::error(vec![Content::text(format!(
                "Tool not found: {tool_name}"
            ))])),
        }
    }
}

pub struct CallCatalogTool {
    sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
    top_level: Arc<RwLock<std::collections::HashMap<String, Arc<dyn TopLevelTool>>>>,
    tool_caching: Option<Arc<ToolCaching>>,
}

impl CallCatalogTool {
    pub fn new(
        sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
        top_level: Arc<RwLock<std::collections::HashMap<String, Arc<dyn TopLevelTool>>>>,
        tool_caching: Option<Arc<ToolCaching>>,
    ) -> Self {
        Self {
            sets,
            top_level,
            tool_caching,
        }
    }

    fn find_set(
        &self,
        subject: &AuthSubject,
        prefixed_name: &str,
    ) -> Option<(Arc<dyn SearchableToolSet>, String)> {
        let sets = self.sets.read().expect("toolset lock poisoned");
        super::super::dispatch::find_searchable(sets.iter(), subject, prefixed_name)
    }

    /// Top-level tools that models route through call_tool (audit:
    /// tool_output_fetch, submit_output, workflow). Hidden tools stay
    /// hidden — the proxy must not be a side door past `is_visible`
    /// (e.g. bash for read-only agents). Self-dispatch is refused.
    fn find_top_level(&self, subject: &AuthSubject, name: &str) -> Option<Arc<dyn TopLevelTool>> {
        if name == "call_tool" {
            return None;
        }
        let map = self.top_level.read().expect("top_level lock poisoned");
        map.get(name).filter(|t| t.is_visible(subject)).cloned()
    }

    fn not_found(&self, subject: &AuthSubject, requested: &str) -> ToolSetsError {
        let mut candidates: Vec<String> = {
            let sets = self.sets.read().expect("toolset lock poisoned");
            visible_entries(subject, &sets)
                .into_iter()
                .map(|e| e.prefixed_name)
                .collect()
        };
        {
            let map = self.top_level.read().expect("top_level lock poisoned");
            candidates.extend(
                map.values()
                    .filter(|t| t.is_visible(subject))
                    .map(|t| t.name().to_string()),
            );
        }
        let suggestions = closest_tool_names(requested, &candidates);
        let msg = if suggestions.is_empty() {
            format!("{requested} — no such tool; use search_tools to discover available tools")
        } else {
            format!(
                "{requested} — no such tool. Did you mean: {}? \
                 Use search_tools to discover available tools.",
                suggestions.join(", ")
            )
        };
        ToolSetsError::ToolNotFound(msg)
    }
}

/// Models confuse naming conventions (audit: `github.pull_request_read`,
/// `mcp__drua__tool_output_fetch`) — map those spellings back to the
/// canonical prefixed name before declaring a miss.
fn canonicalize_tool_name(name: &str) -> String {
    let stripped = name.strip_prefix("mcp__drua__").unwrap_or(name);
    stripped.replace('.', "_")
}

fn name_tokens(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(['_', '-', '.', ' '])
        .filter(|t| !t.is_empty())
        .map(String::from)
        .collect()
}

fn closest_tool_names(requested: &str, candidates: &[String]) -> Vec<String> {
    let req = name_tokens(requested);
    if req.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(usize, &String)> = candidates
        .iter()
        .filter_map(|c| {
            let toks = name_tokens(c);
            let score = req.iter().filter(|t| toks.contains(t)).count();
            (score > 0).then_some((score, c))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    scored.into_iter().take(3).map(|(_, c)| c.clone()).collect()
}

static CALL_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "tool_name": {
                "type": "string",
                "description": "The prefixed tool name returned from search_tools (e.g. 'honeycomb_list_environments')"
            },
            "arguments": {
                "type": "object",
                "description": "Tool arguments matching the schema from describe_tool"
            }
        },
        "required": ["tool_name"]
    })
});

#[async_trait::async_trait]
impl TopLevelTool for CallCatalogTool {
    fn name(&self) -> &str {
        "call_tool"
    }
    fn description(&self) -> &str {
        "Execute an upstream tool by its prefixed name with the provided \
         arguments. Use describe_tool first to understand the parameters. \
         Oversize results are auto-classified and persisted; recover full \
         bytes via tool_output_fetch(invocation_id, query)."
    }
    fn input_schema(&self) -> &serde_json::Value {
        &CALL_SCHEMA
    }

    fn default_tool_caching(&self) -> bool {
        false
    }

    fn composable(&self) -> bool {
        false
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let mut args = arguments.unwrap_or_default();
        let tool_name = args
            .remove("tool_name")
            .and_then(|v| v.as_str().map(str::to_string))
            .ok_or_else(|| {
                let received: Vec<&String> = args.keys().collect();
                let msg = if received.is_empty() {
                    "missing required field `tool_name` — call_tool envelope is \
                     {tool_name: \"<prefixed name from search_tools>\", arguments: {…}}"
                        .to_string()
                } else {
                    format!(
                        "missing required field `tool_name`; received keys: {received:?}. \
                         call_tool envelope is {{tool_name: \"<prefixed name from \
                         search_tools>\", arguments: {{…}}}} — put the tool's own \
                         parameters inside `arguments`."
                    )
                };
                ToolSetsError::InvalidArgument(msg)
            })?;
        let inner_args = args.remove("arguments").and_then(|v| match v {
            serde_json::Value::Object(obj) => Some(obj),
            _ => None,
        });

        let extra_keys: Vec<String> = args.keys().cloned().collect();

        let canonical = canonicalize_tool_name(&tool_name);
        let (set, name) = match self
            .find_set(subject, &tool_name)
            .or_else(|| self.find_set(subject, &canonical))
        {
            Some(found) => found,
            None => {
                if let Some(tool) = self.find_top_level(subject, &canonical) {
                    Audit::record_action(tool.name());
                    // Same arg-coercion treatment as direct top-level dispatch.
                    let inner_args = inner_args.map(|mut a| {
                        super::super::auto_parse_args::coerce_args_to_schema(
                            &mut a,
                            tool.input_schema(),
                        );
                        a
                    });
                    return tool.call(subject, inner_args).await;
                }
                return Err(self.not_found(subject, &tool_name));
            }
        };

        // Auto-parse inner_args against the upstream tool's input schema —
        // the outer call_tool envelope only declares `arguments: object`, so
        // stringified JSON inside individual upstream-arg fields wouldn't
        // otherwise be parsed.
        let mut inner_args = inner_args.map(|mut a| {
            if let Some(entry) = set.tools().iter().find(|t| t.name == name) {
                let schema =
                    serde_json::Value::Object(entry.description.input_schema.as_ref().clone());
                super::super::auto_parse_args::coerce_args_to_schema(&mut a, &schema);
            }
            a
        });

        Audit::record_action(format!("catalog: {}", tool_name));

        let mut result = set.call(subject, &name, inner_args.clone()).await;

        // Recover a double-wrapped `{arguments: {…}}` payload — some models
        // nest the upstream args again inside call_tool's own `arguments`.
        // If the call failed and the inner args are exactly that wrapper (and
        // the upstream tool doesn't declare its own `arguments` field), retry
        // once with the unwrapped object.
        if crate::toolset::call_failed(&result) {
            let upstream_schema = set
                .tools()
                .iter()
                .find(|t| t.name == name)
                .map(|t| serde_json::Value::Object(t.description.input_schema.as_ref().clone()))
                .unwrap_or_else(|| serde_json::json!({}));
            if let Some(unwrapped) = inner_args
                .as_ref()
                .and_then(|a| crate::arguments_envelope::strip_for_dispatch(a, &upstream_schema))
            {
                let retry = set.call(subject, &name, Some(unwrapped.clone())).await;
                if !crate::toolset::call_failed(&retry) {
                    tracing::warn!(
                        tool = %tool_name,
                        "recovered from `arguments` envelope; retried upstream with unwrapped args",
                    );
                    result = retry;
                    inner_args = Some(unwrapped);
                }
            }
        }

        let result = annotate_envelope_mistake(result, &tool_name, &extra_keys)?;

        let result = match self.tool_caching.as_ref() {
            Some(tc) => {
                let args_for_cache = inner_args
                    .map(serde_json::Value::Object)
                    .unwrap_or_else(|| serde_json::Value::Object(Default::default()));
                tc.cache(subject, &tool_name, &args_for_cache, result)
                    .await?
                    .result
            }
            None => result,
        };
        Ok(result)
    }
}

fn annotate_envelope_mistake(
    result: Result<CallToolResult, ToolSetsError>,
    tool_name: &str,
    extra_keys: &[String],
) -> Result<CallToolResult, ToolSetsError> {
    if extra_keys.is_empty() {
        return result;
    }
    let Err(err) = result else {
        return result;
    };
    let stray = extra_keys.join(", ");
    let raw = err.to_string();
    Err(ToolSetsError::InvalidArgument(format!(
        "{raw}\nHint: call_tool envelope is {{tool_name, arguments:{{…}}}}; \
         these top-level fields were ignored: {stray}. \
         Likely you wrote `call_tool({{tool_name:\"{tool_name}\", {stray}: …}})` — \
         move them inside `arguments`: \
         `call_tool({{tool_name:\"{tool_name}\", arguments:{{{stray}: …}}}})`."
    )))
}

fn visible_entries(
    subject: &AuthSubject,
    sets: &[Arc<dyn SearchableToolSet>],
) -> Vec<CatalogEntry> {
    let mut out = Vec::new();
    for set in sets.iter() {
        if !set.is_visible(subject) {
            continue;
        }
        for tool in set.tools() {
            let desc = &tool.description;
            let brief = desc
                .description
                .as_ref()
                .map(|d| brief_description(d))
                .unwrap_or_default();
            out.push(CatalogEntry {
                prefixed_name: format!("{}_{}", set.prefix(), desc.name),
                upstream_name: set.name().to_string(),
                tool_name: desc.name.to_string(),
                category: set.category().to_string(),
                brief_description: brief,
                full_tool: desc.clone(),
            });
        }
    }
    out
}

const MAX_BRIEF_DESCRIPTION_CHARS: usize = 220;

fn brief_description(s: &str) -> String {
    let first_paragraph = s
        .split("\n\n")
        .find(|part| !part.trim().is_empty())
        .unwrap_or(s);
    let compact = first_paragraph
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let compact_chars = compact.chars().count();
    if compact_chars <= MAX_BRIEF_DESCRIPTION_CHARS + 3 {
        return compact;
    }

    let mut end = 0;
    for (idx, _) in compact.char_indices() {
        if compact[..idx].chars().count() > MAX_BRIEF_DESCRIPTION_CHARS {
            break;
        }
        if compact[..idx].ends_with(' ') {
            end = idx;
        }
    }
    if end == 0 {
        end = compact
            .char_indices()
            .nth(MAX_BRIEF_DESCRIPTION_CHARS)
            .map(|(idx, _)| idx)
            .unwrap_or(compact.len());
    }
    format!("{}...", compact[..end].trim_end())
}

#[cfg(test)]
mod tests {
    use rmcp::model::{CallToolResult, JsonObject};

    use super::super::super::error::ToolSetsError;
    use super::super::super::traits::ToolSetEntry;
    use super::*;

    struct StubToolSet {
        entries: Vec<ToolSetEntry>,
    }

    impl StubToolSet {
        fn with_tool(name: &str, description: &str) -> Self {
            Self::with_tools(vec![(name, description)])
        }

        fn with_tools(tools: Vec<(&str, &str)>) -> Self {
            Self {
                entries: tools
                    .into_iter()
                    .map(|(name, desc)| {
                        let tool =
                            Tool::new(name.to_string(), desc.to_string(), JsonObject::default());
                        ToolSetEntry {
                            name: name.to_string(),
                            description: tool,
                        }
                    })
                    .collect(),
            }
        }

        fn with_typed_tool(
            name: &str,
            description: &str,
            input_schema: serde_json::Value,
            output_schema: serde_json::Value,
        ) -> Self {
            let input: JsonObject = match input_schema {
                serde_json::Value::Object(m) => m,
                _ => Default::default(),
            };
            let out = match output_schema {
                serde_json::Value::Object(m) => Some(Arc::new(m)),
                _ => None,
            };
            let mut tool = Tool::default();
            tool.name = name.to_string().into();
            tool.description = Some(description.to_string().into());
            tool.input_schema = Arc::new(input);
            tool.output_schema = out;
            Self {
                entries: vec![ToolSetEntry {
                    name: name.to_string(),
                    description: tool,
                }],
            }
        }
    }

    #[async_trait::async_trait]
    impl SearchableToolSet for StubToolSet {
        fn name(&self) -> &str {
            "stub"
        }
        fn category(&self) -> &str {
            "test"
        }
        fn category_description(&self) -> &str {
            "Test toolset"
        }
        fn tools(&self) -> &[ToolSetEntry] {
            &self.entries
        }
        async fn call(
            &self,
            _subject: &AuthSubject,
            _tool_name: &str,
            _arguments: Option<JsonObject>,
        ) -> Result<CallToolResult, ToolSetsError> {
            unimplemented!()
        }
    }

    fn search_catalog(stubs: Vec<StubToolSet>) -> SearchCatalog {
        let sets: Vec<Arc<dyn SearchableToolSet>> = stubs
            .into_iter()
            .map(|s| Arc::new(s) as Arc<dyn SearchableToolSet>)
            .collect();
        SearchCatalog::new(Arc::new(RwLock::new(sets)))
    }

    #[test]
    fn search_ranks_by_keyword_hits() {
        let catalog = search_catalog(vec![StubToolSet::with_tools(vec![
            ("get_pipeline_status", "Get pipeline build status"),
            ("list_pipelines", "List CI pipelines"),
            ("search_code", "Semantic search over indexed codebases"),
        ])]);

        let results =
            catalog.execute_search(&AuthSubject::Anonymous, Some("pipeline status"), None);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].tool_name, "get_pipeline_status");
        assert_eq!(results[1].tool_name, "list_pipelines");
    }

    #[test]
    fn search_individual_keywords_match_across_name() {
        let catalog = search_catalog(vec![StubToolSet::with_tool(
            "get_pipeline_status",
            "Returns the current status of a CI pipeline",
        )]);

        let results =
            catalog.execute_search(&AuthSubject::Anonymous, Some("pipeline status"), None);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].tool_name, "get_pipeline_status");
    }

    #[test]
    fn search_normalizes_underscores_and_hyphens() {
        let catalog = search_catalog(vec![StubToolSet::with_tool(
            "search_code",
            "Semantic search over indexed codebases",
        )]);

        for query in ["search code", "search_code", "search-code"] {
            let results = catalog.execute_search(&AuthSubject::Anonymous, Some(query), None);
            assert_eq!(results.len(), 1, "query '{query}' should match");
            assert_eq!(results[0].tool_name, "search_code");
        }
    }

    #[test]
    fn search_single_keyword_matches() {
        let catalog = search_catalog(vec![StubToolSet::with_tools(vec![
            ("list_pipelines", "List CI pipelines"),
            ("get_build_log", "Get build output"),
        ])]);

        let results = catalog.execute_search(&AuthSubject::Anonymous, Some("pipeline"), None);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].tool_name, "list_pipelines");
    }

    #[test]
    fn search_no_match_returns_empty() {
        let catalog = search_catalog(vec![StubToolSet::with_tool(
            "search_code",
            "Semantic search over indexed codebases",
        )]);

        let results = catalog.execute_search(&AuthSubject::Anonymous, Some("nonexistent"), None);
        assert!(results.is_empty());
    }

    #[test]
    fn describe_tool_includes_ts_signature() {
        let stub = StubToolSet::with_typed_tool(
            "list_envs",
            "List environments.",
            json!({
                "type": "object",
                "properties": {
                    "limit": { "type": "integer" },
                    "name": { "type": "string" }
                },
                "required": ["name"]
            }),
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "count": { "type": "integer" }
                },
                "required": ["id", "count"]
            }),
        );

        let sets: Vec<Arc<dyn SearchableToolSet>> =
            vec![Arc::new(stub) as Arc<dyn SearchableToolSet>];
        let describe = DescribeCatalogTool::new(Arc::new(RwLock::new(sets)));
        let entry = describe
            .execute_describe(&AuthSubject::Anonymous, "stub_list_envs")
            .expect("entry should be visible");
        let formatted = DescribeCatalogTool::format_entry(&entry);

        assert!(
            formatted.contains("### TypeScript signature"),
            "missing TS signature heading in:\n{formatted}"
        );
        assert!(
            formatted.contains("function list_envs(args: {"),
            "missing function declaration in:\n{formatted}"
        );
        assert!(
            formatted.contains("name: string"),
            "missing required string param in:\n{formatted}"
        );
        assert!(
            formatted.contains("limit?: number"),
            "missing optional number param in:\n{formatted}"
        );
        assert!(
            formatted.contains("Promise<{"),
            "missing Promise wrapper in:\n{formatted}"
        );
        assert!(
            formatted.contains("id: string"),
            "missing return field in:\n{formatted}"
        );
    }

    #[test]
    fn brief_description_preserves_abbreviations() {
        let brief = brief_description(
            "Search indexed codebases for code patterns matching a query, e.g. pass `fn main` for Rust functions.\n\nUsage tips:\n- Prefer concrete snippets.",
        );

        assert_eq!(
            brief,
            "Search indexed codebases for code patterns matching a query, e.g. pass `fn main` for Rust functions."
        );
    }

    #[test]
    fn brief_description_compacts_and_truncates_on_word_boundary() {
        let long = format!("{}tail", "alpha ".repeat(60));
        let brief = brief_description(&long);

        assert!(brief.ends_with("..."), "{brief}");
        assert!(brief.chars().count() <= MAX_BRIEF_DESCRIPTION_CHARS + 3);
        assert!(!brief.contains("tail"));
    }

    #[test]
    fn brief_description_does_not_truncate_to_longer_output() {
        let input = "x".repeat(MAX_BRIEF_DESCRIPTION_CHARS + 1);
        let brief = brief_description(&input);

        assert_eq!(brief, input);
    }

    #[test]
    fn normalize_replaces_underscores_and_hyphens() {
        assert_eq!(SearchCatalog::normalize("search_code"), "search code");
        assert_eq!(SearchCatalog::normalize("list-pipelines"), "list pipelines");
        assert_eq!(SearchCatalog::normalize("Search_Code"), "search code");
        assert_eq!(SearchCatalog::normalize("no changes"), "no changes");
    }

    #[test]
    fn envelope_mistake_hint_is_prepended_when_extra_keys_present() {
        let inner = Err(ToolSetsError::InvalidArgument(
            "missing field `build_id`".to_string(),
        ));
        let wrapped =
            annotate_envelope_mistake(inner, "concourse_get_build_logs", &["build_id".to_string()]);
        let msg = wrapped.unwrap_err().to_string();
        assert!(
            msg.contains("missing field `build_id`"),
            "underlying error preserved: {msg}"
        );
        assert!(
            msg.contains("call_tool envelope"),
            "envelope hint added: {msg}"
        );
        assert!(
            msg.contains("arguments:{build_id: …}"),
            "fix shape suggested: {msg}"
        );
    }

    #[test]
    fn envelope_mistake_passthrough_when_no_extra_keys() {
        let inner = Err(ToolSetsError::InvalidArgument("oops".to_string()));
        let out = annotate_envelope_mistake(inner, "concourse_get_build_logs", &[]);
        assert_eq!(
            out.unwrap_err().to_string(),
            "ToolSetsError - InvalidArgument: oops"
        );
    }

    #[test]
    fn envelope_mistake_passthrough_on_success() {
        let ok: Result<CallToolResult, ToolSetsError> =
            Ok(CallToolResult::success(vec![Content::text("ok")]));
        let out = annotate_envelope_mistake(ok, "x", &["stray".to_string()]);
        assert!(out.is_ok());
    }

    #[tokio::test]
    async fn missing_tool_name_echoes_received_keys() {
        let sets: Vec<Arc<dyn SearchableToolSet>> = vec![];
        let call = CallCatalogTool::new(
            Arc::new(RwLock::new(sets)),
            Arc::new(RwLock::new(Default::default())),
            None,
        );

        let err = call
            .call(&AuthSubject::Anonymous, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("InvalidArgument"), "got: {err}");
        assert!(
            err.contains("missing required field `tool_name`"),
            "got: {err}"
        );

        let mut args = JsonObject::default();
        args.insert("incident_id".to_string(), json!("123"));
        let err = call
            .call(&AuthSubject::Anonymous, Some(args))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("received keys: [\"incident_id\"]"),
            "got: {err}"
        );
        assert!(err.contains("inside `arguments`"), "got: {err}");
    }

    #[test]
    fn canonicalize_handles_dots_and_mcp_prefix() {
        assert_eq!(
            canonicalize_tool_name("github.pull_request_read"),
            "github_pull_request_read"
        );
        assert_eq!(
            canonicalize_tool_name("mcp__drua__tool_output_fetch"),
            "tool_output_fetch"
        );
        assert_eq!(
            canonicalize_tool_name("zenduty_get_incident"),
            "zenduty_get_incident"
        );
    }

    #[test]
    fn closest_tool_names_ranks_by_shared_tokens() {
        let candidates = vec![
            "github_pull_request_read".to_string(),
            "github_get_file_contents".to_string(),
            "zenduty_get_incident".to_string(),
        ];
        let got = closest_tool_names("github_pull_request_review_write", &candidates);
        assert_eq!(got[0], "github_pull_request_read");
        assert!(!got.contains(&"zenduty_get_incident".to_string()));

        assert!(closest_tool_names("xyzzy", &candidates).is_empty());
    }

    #[tokio::test]
    async fn unknown_tool_error_suggests_close_names() {
        let stub = StubToolSet::with_tools(vec![("pull_request_read", "Read a PR")]);
        let sets: Vec<Arc<dyn SearchableToolSet>> = vec![Arc::new(stub)];
        let call = CallCatalogTool::new(
            Arc::new(RwLock::new(sets)),
            Arc::new(RwLock::new(Default::default())),
            None,
        );

        let mut args = JsonObject::default();
        args.insert("tool_name".to_string(), json!("stub.pull_request_reed"));
        let err = call
            .call(&AuthSubject::Anonymous, Some(args))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Did you mean"), "got: {err}");
        assert!(err.contains("stub_pull_request_read"), "got: {err}");
    }

    struct StubTopLevel {
        schema: serde_json::Value,
        visible: bool,
    }

    #[async_trait::async_trait]
    impl TopLevelTool for StubTopLevel {
        fn name(&self) -> &str {
            "submit_output"
        }
        fn description(&self) -> &str {
            "stub"
        }
        fn input_schema(&self) -> &serde_json::Value {
            &self.schema
        }
        fn is_visible(&self, _subject: &AuthSubject) -> bool {
            self.visible
        }
        async fn call(
            &self,
            _subject: &AuthSubject,
            arguments: Option<JsonObject>,
        ) -> Result<CallToolResult, ToolSetsError> {
            let echo = serde_json::to_string(&arguments.unwrap_or_default()).unwrap();
            Ok(CallToolResult::success(vec![Content::text(echo)]))
        }
    }

    fn call_tool_with_stub_top_level(visible: bool) -> CallCatalogTool {
        let mut top_level: std::collections::HashMap<String, Arc<dyn TopLevelTool>> =
            Default::default();
        top_level.insert(
            "submit_output".to_string(),
            Arc::new(StubTopLevel {
                schema: json!({"properties": {"len": {"type": "integer"}}}),
                visible,
            }),
        );
        let sets: Vec<Arc<dyn SearchableToolSet>> = vec![];
        CallCatalogTool::new(
            Arc::new(RwLock::new(sets)),
            Arc::new(RwLock::new(top_level)),
            None,
        )
    }

    fn call_tool_args(tool_name: &str) -> JsonObject {
        let mut args = JsonObject::default();
        args.insert("tool_name".to_string(), json!(tool_name));
        args
    }

    #[tokio::test]
    async fn call_tool_proxies_registered_top_level_tools() {
        let call = call_tool_with_stub_top_level(true);

        for requested in ["submit_output", "mcp__drua__submit_output"] {
            let result = call
                .call(&AuthSubject::Anonymous, Some(call_tool_args(requested)))
                .await
                .unwrap();
            assert_ne!(result.is_error, Some(true), "requested: {requested}");
        }

        // Self-dispatch is refused, not recursed.
        assert!(call
            .call(&AuthSubject::Anonymous, Some(call_tool_args("call_tool")))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn call_tool_proxy_coerces_args_against_tool_schema() {
        let call = call_tool_with_stub_top_level(true);

        let mut args = call_tool_args("submit_output");
        args.insert("arguments".to_string(), json!({"len": "200"}));
        let result = call
            .call(&AuthSubject::Anonymous, Some(args))
            .await
            .unwrap();
        let echoed = result.content[0].as_text().unwrap().text.clone();
        assert!(echoed.contains("\"len\":200"), "got: {echoed}");
    }

    #[tokio::test]
    async fn call_tool_does_not_proxy_hidden_top_level_tools() {
        let call = call_tool_with_stub_top_level(false);

        let err = call
            .call(
                &AuthSubject::Anonymous,
                Some(call_tool_args("submit_output")),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("ToolNotFound"), "got: {err}");
    }

    /// Strict MCP clients (e.g. Claude Code) reject boolean schemas inside
    /// `properties`. `serde_json::Value` fields must serialize as `{}`, not
    /// `true`.
    #[test]
    fn describe_output_schema_has_no_boolean_properties() {
        let props = DESCRIBE_OUTPUT_SCHEMA
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("describe outputSchema has properties");
        for (name, schema) in props {
            assert!(
                !matches!(schema, serde_json::Value::Bool(_)),
                "property `{name}` is a boolean schema, MCP validators reject it"
            );
        }
    }

    /// `output_schema` is `Option<_>` and omitted when a tool has no upstream
    /// output schema (the common case). It MUST NOT appear in the schema's
    /// `required`, or strict MCP clients reject the `structuredContent` of
    /// every such describe_tool call with "data must have required property
    /// 'output_schema'". `schema_with` on an `Option` field defeats schemars
    /// 0.8's Option-detection, so `#[schemars(default)]` restores it.
    #[test]
    fn describe_output_schema_does_not_require_output_schema_field() {
        let required = DESCRIBE_OUTPUT_SCHEMA
            .get("required")
            .and_then(|v| v.as_array())
            .expect("describe outputSchema has a required array");
        let required_names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            !required_names.contains(&"output_schema"),
            "`output_schema` must not be required (it is Option and omitted for \
             most tools), but schema requires: {required_names:?}"
        );
        // Sanity: the always-present fields are still required.
        for always in [
            "name",
            "upstream",
            "category",
            "description",
            "input_schema",
        ] {
            assert!(
                required_names.contains(&always),
                "`{always}` should be required, got: {required_names:?}"
            );
        }
    }
}
