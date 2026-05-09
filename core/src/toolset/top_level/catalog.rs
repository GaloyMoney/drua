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

use serde_json::json;

use rmcp::model::{CallToolResult, Content, JsonObject, Tool};

use crate::audit::Audit;
use crate::auth::AuthSubject;

use super::super::error::ToolSetsError;
use super::super::traits::{SearchableToolSet, TopLevelTool};
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
    #[serde(skip_serializing_if = "Option::is_none")]
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
    fn output_schema(&self) -> Option<&serde_json::Value> {
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
        let output_section = tool
            .output_schema
            .as_ref()
            .and_then(|s| serde_json::to_string_pretty(s.as_ref()).ok())
            .map(|s| format!("\n\n### Output schema\n```json\n{s}\n```"))
            .unwrap_or_default();

        // Embed a TS signature so agents writing `compose` scripts can read the typed
        // shape inline; `compose_types` remains useful for batch/prefix-glob lookups.
        let input_schema_value = serde_json::Value::Object(tool.input_schema.as_ref().clone());
        let input_ts = json_schema_ts::schema_to_ts_params(&input_schema_value);
        let output_ts = tool
            .output_schema
            .as_ref()
            .map(|s| {
                let v = serde_json::Value::Object(s.as_ref().clone());
                json_schema_ts::schema_to_ts(&v)
            })
            .unwrap_or_else(|| "any".to_string());
        let ts_signature = format!(
            "\n\n### TypeScript signature (for use in `compose`)\n```ts\nfunction {tool}(args: {{ {input_ts} }}): Promise<{output_ts}>;\n```",
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
    fn output_schema(&self) -> Option<&serde_json::Value> {
        Some(&DESCRIBE_OUTPUT_SCHEMA)
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
                    output_schema: tool
                        .output_schema
                        .as_ref()
                        .map(|s| serde_json::Value::Object(s.as_ref().clone())),
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
    tool_invocations: Option<Arc<super::super::tool_invocations::ToolInvocations>>,
    classifiers: Arc<super::super::classifier::ClassifierRegistry>,
}

impl CallCatalogTool {
    pub fn new(
        sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
        tool_invocations: Option<Arc<super::super::tool_invocations::ToolInvocations>>,
        classifiers: Arc<super::super::classifier::ClassifierRegistry>,
    ) -> Self {
        Self {
            sets,
            tool_invocations,
            classifiers,
        }
    }

    fn find_set(
        &self,
        subject: &AuthSubject,
        prefixed_name: &str,
    ) -> Option<(Arc<dyn SearchableToolSet>, String)> {
        let sets = self.sets.read().expect("toolset lock poisoned");
        for set in sets.iter() {
            if !set.is_visible(subject) {
                continue;
            }
            let prefix = format!("{}_", set.prefix());
            if let Some(tool_name) = prefixed_name.strip_prefix(&prefix) {
                if set.tools().iter().any(|t| t.name == tool_name) {
                    return Some((Arc::clone(set), tool_name.to_string()));
                }
            }
        }
        None
    }
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

    fn bypass_universal_pipeline(&self) -> bool {
        true
    }

    fn records_own_pipeline_metrics(&self) -> bool {
        true
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
            .ok_or_else(|| ToolSetsError::ToolNotFound("missing tool_name".to_string()))?;
        let inner_args = args.remove("arguments").and_then(|v| match v {
            serde_json::Value::Object(obj) => Some(obj),
            _ => None,
        });

        let extra_keys: Vec<String> = args.keys().cloned().collect();

        let (set, name) = self
            .find_set(subject, &tool_name)
            .ok_or_else(|| ToolSetsError::ToolNotFound(tool_name.clone()))?;

        Audit::record_action(format!("catalog: {}", tool_name));

        let started_at = chrono::Utc::now();
        let start = std::time::Instant::now();
        let result = set.call(subject, &name, inner_args.clone()).await;
        let duration_ms = start.elapsed().as_millis() as u64;

        let result = annotate_envelope_mistake(result, &tool_name, &extra_keys)?;

        let recorded_args = inner_args
            .map(serde_json::Value::Object)
            .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));

        let Some(invocations) = self.tool_invocations.as_ref() else {
            let raw_bytes = super::super::estimate_text_bytes(&result);
            super::super::record_pipeline_metrics(raw_bytes, raw_bytes, "bypass", false);
            return Ok(result);
        };
        let classification =
            self.classifiers
                .classify(&super::super::classifier::ClassifierContext {
                    tool_name: &tool_name,
                    args: &recorded_args,
                    raw: &result,
                    exit_code: None,
                });
        let raw_bytes = classification.canonical_text.len() as u64;
        let classifier_kind = classification.summary.kind().to_string();
        let kept_bytes = classification.summary.kept_bytes(raw_bytes);
        if classification.summary.is_passthrough() {
            super::super::record_pipeline_metrics(raw_bytes, kept_bytes, &classifier_kind, false);
            return Ok(result);
        }
        let Some(owner) = super::super::invocation_owner(subject) else {
            super::super::record_pipeline_metrics(raw_bytes, kept_bytes, &classifier_kind, false);
            return Ok(result);
        };
        let outcome = invocations
            .persist_and_envelope(
                owner,
                &tool_name,
                &recorded_args,
                classification,
                &result,
                duration_ms,
                started_at,
            )
            .await;
        let persisted = outcome.is_some();
        super::super::record_pipeline_metrics(raw_bytes, kept_bytes, &classifier_kind, persisted);
        match outcome {
            Some((wrapped, invocation_id)) => {
                crate::audit::Audit::record_tool_invocation_id(invocation_id);
                Ok(wrapped)
            }
            None => Ok(result),
        }
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
                .map(|d| first_sentence(d))
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

fn first_sentence(s: &str) -> String {
    s.split_once(". ")
        .or_else(|| s.split_once(".\n"))
        .map(|(first, _)| format!("{first}."))
        .unwrap_or_else(|| s.to_string())
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
}
