//! Catalog-backed meta-tools: `search_tools`, `describe_tool`, and
//! `call_tool`. The first two are read-only and skip auth/audit; `call_tool`
//! mutates upstream services so it threads `auth` through and uses the
//! [`dispatch_tool_call`](super::super::dispatch_tool_call) path for scope
//! checks + audit.
//!
//! The catalog query helpers ([`search`], [`describe`], [`find_set`], ...)
//! live in this file because the only consumers are these three meta-tools
//! plus `dispatch_tool_call` (via `pub(super)` re-export). They operate on a
//! borrowed slice of toolsets; callers acquire the read lock and drop it
//! before any `.await`.

use std::sync::{Arc, LazyLock, RwLock};

use rmcp::model::{CallToolResult, Content, JsonObject, Tool};

use super::super::error::ToolSetsError;
use super::super::filter::OutputFilter;
use super::super::traits::{SearchableToolSet, TopLevelTool};

// ---------------------------------------------------------------------------
// Catalog query helpers
// ---------------------------------------------------------------------------

pub struct CatalogEntry {
    pub prefixed_name: String,
    pub upstream_name: String,
    pub tool_name: String,
    pub category: String,
    pub brief_description: String,
    pub full_tool: Tool,
    pub default_output_filter: Option<OutputFilter>,
}

/// Format search results as text grouped by category.
fn format_search_results(results: &[CatalogEntry]) -> String {
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

/// Format a catalog entry as a detailed markdown description.
fn format_describe(entry: &CatalogEntry) -> String {
    let tool = &entry.full_tool;
    let description = tool
        .description
        .as_deref()
        .unwrap_or("No description available.");
    let schema =
        serde_json::to_string_pretty(&tool.input_schema).unwrap_or_else(|_| "{}".into());
    let filter_desc = match &entry.default_output_filter {
        Some(f) => format!("tool default: {}", f.describe()),
        None => format!(
            "global default: {}",
            OutputFilter::global_default().describe()
        ),
    };
    format!(
        "## {}\n\nUpstream: {}\nCategory: {}\n\n{}\n\n### Parameters\n```json\n{}\n```\n\n### Default output filter\n{}\n\nUse call_tool(\"{}\", {{...}}) to execute.",
        entry.prefixed_name,
        entry.upstream_name,
        entry.category,
        description,
        schema,
        filter_desc,
        entry.prefixed_name,
    )
}

fn entries(sets: &[Arc<dyn SearchableToolSet>]) -> Vec<CatalogEntry> {
    let mut out = Vec::new();
    for set in sets.iter() {
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
                default_output_filter: tool.default_output_filter.clone(),
            });
        }
    }
    out
}

fn search(
    sets: &[Arc<dyn SearchableToolSet>],
    query: Option<&str>,
    category: Option<&str>,
) -> Vec<CatalogEntry> {
    let mut entries: Vec<CatalogEntry> = entries(sets)
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
        let keywords: Vec<String> = normalize(q).split_whitespace().map(String::from).collect();
        if !keywords.is_empty() {
            let mut scored: Vec<_> = entries
                .into_iter()
                .filter_map(|e| {
                    let score = keyword_score(&e, &keywords);
                    if score > 0 {
                        Some((score, e))
                    } else {
                        None
                    }
                })
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0));
            entries = scored.into_iter().map(|(_, e)| e).collect();
        }
    }

    entries
}

fn describe(
    sets: &[Arc<dyn SearchableToolSet>],
    prefixed_name: &str,
) -> Option<CatalogEntry> {
    entries(sets)
        .into_iter()
        .find(|e| e.prefixed_name == prefixed_name)
}

/// Find the toolset, stripped tool name, and default output filter for a
/// prefixed tool name. Returns an `Arc` clone so callers can drop the lock
/// before any `.await`. Pure name lookup — no scope filtering.
///
/// `pub(in super::super)` so [`super::super::dispatch_tool_call`] can use it.
pub(in super::super) fn find_set(
    sets: &[Arc<dyn SearchableToolSet>],
    prefixed_name: &str,
) -> Option<(Arc<dyn SearchableToolSet>, String, Option<OutputFilter>)> {
    for set in sets.iter() {
        let prefix = format!("{}_", set.prefix());
        if let Some(tool_name) = prefixed_name.strip_prefix(&prefix) {
            if let Some(entry) = set.tools().iter().find(|t| t.name == tool_name) {
                return Some((
                    Arc::clone(set),
                    tool_name.to_string(),
                    entry.default_output_filter.clone(),
                ));
            }
        }
    }
    None
}

/// Normalize a string for fuzzy keyword matching: lowercase and collapse
/// underscores/hyphens into spaces so "search code", "search_code", and
/// "search-code" all match each other.
fn normalize(s: &str) -> String {
    s.to_lowercase().replace(['_', '-'], " ")
}

/// Score a catalog entry against a set of keywords.
/// Returns the count of query keywords found in the entry's searchable text.
fn keyword_score(entry: &CatalogEntry, keywords: &[String]) -> usize {
    let haystack = [
        normalize(&entry.tool_name),
        normalize(&entry.upstream_name),
        normalize(&entry.brief_description),
    ]
    .join(" ");
    keywords
        .iter()
        .filter(|kw| haystack.contains(kw.as_str()))
        .count()
}

fn first_sentence(s: &str) -> String {
    s.split_once(". ")
        .or_else(|| s.split_once(".\n"))
        .map(|(first, _)| format!("{first}."))
        .unwrap_or_else(|| s.to_string())
}

// ---------------------------------------------------------------------------
// search_tools
// ---------------------------------------------------------------------------

pub struct SearchCatalog {
    sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
}

impl SearchCatalog {
    pub fn new(sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>) -> Self {
        Self { sets }
    }
}

static SEARCH_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "Free-form search query" },
            "category": { "type": "string", "description": "Optional category filter ('all' for any)" }
        }
    })
});

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

    async fn call(&self, arguments: Option<JsonObject>) -> Result<CallToolResult, ToolSetsError> {
        let args = arguments.as_ref();
        let query = args.and_then(|a| a.get("query")).and_then(|v| v.as_str());
        let category = args
            .and_then(|a| a.get("category"))
            .and_then(|v| v.as_str());
        let results = {
            let sets = self.sets.read().expect("toolset lock poisoned");
            search(&sets, query, category)
        };
        let text = format_search_results(&results);
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}

// ---------------------------------------------------------------------------
// describe_tool
// ---------------------------------------------------------------------------

pub struct DescribeCatalogTool {
    sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
}

impl DescribeCatalogTool {
    pub fn new(sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>) -> Self {
        Self { sets }
    }
}

static DESCRIBE_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "tool_name": { "type": "string", "description": "The prefixed tool name returned from search_tools" }
        },
        "required": ["tool_name"]
    })
});

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

    async fn call(&self, arguments: Option<JsonObject>) -> Result<CallToolResult, ToolSetsError> {
        let tool_name = arguments
            .as_ref()
            .and_then(|a| a.get("tool_name"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let entry = {
            let sets = self.sets.read().expect("toolset lock poisoned");
            describe(&sets, tool_name)
        };
        let text = match entry {
            Some(entry) => format_describe(&entry),
            None => format!("Tool not found: {tool_name}"),
        };
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}

// ---------------------------------------------------------------------------
// call_tool
// ---------------------------------------------------------------------------

/// The call-a-prefixed-tool meta-tool. Unlike `search_tools` / `describe_tool`,
/// this one mutates upstream services, so it threads `auth` through and uses
/// the full [`ToolSets::call_with_filter`](super::super::ToolSets::call_with_filter)
/// path — scope checks + audit recording + the caller-supplied output filter.
pub struct CallCatalogTool {
    sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
}

impl CallCatalogTool {
    pub fn new(sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>) -> Self {
        Self { sets }
    }
}

static CALL_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "tool_name": {
                "type": "string",
                "description": "The prefixed tool name returned from search_tools (e.g. 'honeycomb_list_environments')"
            },
            "arguments": {
                "type": "object",
                "description": "Tool arguments matching the schema from describe_tool"
            },
            "output_filter": {
                "type": "object",
                "description": "Optional post-processing filter applied to the tool's output (head / tail / grep / invert_match / context_lines). Falls back to the tool's default or the global default when omitted."
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
         Supports an optional output_filter to trim large outputs."
    }
    fn input_schema(&self) -> &serde_json::Value {
        &CALL_SCHEMA
    }

    async fn call(&self, arguments: Option<JsonObject>) -> Result<CallToolResult, ToolSetsError> {
        let mut args = arguments.unwrap_or_default();
        let tool_name = args
            .remove("tool_name")
            .and_then(|v| v.as_str().map(str::to_string))
            .ok_or_else(|| ToolSetsError::ToolNotFound("missing tool_name".to_string()))?;
        let inner_args = args.remove("arguments").and_then(|v| match v {
            serde_json::Value::Object(obj) => Some(obj),
            _ => None,
        });
        let output_filter: Option<OutputFilter> = args
            .remove("output_filter")
            .and_then(|v| serde_json::from_value(v).ok());

        let (set, name, tool_default_filter) = {
            let sets = self.sets.read().expect("toolset lock poisoned");
            find_set(&sets, &tool_name)
                .ok_or_else(|| ToolSetsError::ToolNotFound(tool_name.clone()))?
        };

        let result = set.call(&name, inner_args).await;
        let filter = output_filter
            .or(tool_default_filter)
            .unwrap_or_else(OutputFilter::global_default);
        result.and_then(|r| filter.apply(r))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

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
                            default_output_filter: None,
                        }
                    })
                    .collect(),
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
            _tool_name: &str,
            _arguments: Option<JsonObject>,
        ) -> Result<CallToolResult, ToolSetsError> {
            unimplemented!()
        }
    }

    fn sets(stubs: Vec<StubToolSet>) -> Vec<Arc<dyn SearchableToolSet>> {
        stubs
            .into_iter()
            .map(|s| Arc::new(s) as Arc<dyn SearchableToolSet>)
            .collect()
    }

    #[test]
    fn search_ranks_by_keyword_hits() {
        let sets = sets(vec![StubToolSet::with_tools(vec![
            ("get_pipeline_status", "Get pipeline build status"),
            ("list_pipelines", "List CI pipelines"),
            ("search_code", "Semantic search over indexed codebases"),
        ])]);

        let results = search(&sets, Some("pipeline status"), None);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].tool_name, "get_pipeline_status");
        assert_eq!(results[1].tool_name, "list_pipelines");
    }

    #[test]
    fn search_individual_keywords_match_across_name() {
        let sets = sets(vec![StubToolSet::with_tool(
            "get_pipeline_status",
            "Returns the current status of a CI pipeline",
        )]);

        let results = search(&sets, Some("pipeline status"), None);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].tool_name, "get_pipeline_status");
    }

    #[test]
    fn search_normalizes_underscores_and_hyphens() {
        let sets = sets(vec![StubToolSet::with_tool(
            "search_code",
            "Semantic search over indexed codebases",
        )]);

        for query in ["search code", "search_code", "search-code"] {
            let results = search(&sets, Some(query), None);
            assert_eq!(results.len(), 1, "query '{query}' should match");
            assert_eq!(results[0].tool_name, "search_code");
        }
    }

    #[test]
    fn search_single_keyword_matches() {
        let sets = sets(vec![StubToolSet::with_tools(vec![
            ("list_pipelines", "List CI pipelines"),
            ("get_build_log", "Get build output"),
        ])]);

        let results = search(&sets, Some("pipeline"), None);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].tool_name, "list_pipelines");
    }

    #[test]
    fn search_no_match_returns_empty() {
        let sets = sets(vec![StubToolSet::with_tool(
            "search_code",
            "Semantic search over indexed codebases",
        )]);

        let results = search(&sets, Some("nonexistent"), None);
        assert!(results.is_empty());
    }

    #[test]
    fn normalize_replaces_underscores_and_hyphens() {
        assert_eq!(normalize("search_code"), "search code");
        assert_eq!(normalize("list-pipelines"), "list pipelines");
        assert_eq!(normalize("Search_Code"), "search code");
        assert_eq!(normalize("no changes"), "no changes");
    }
}
