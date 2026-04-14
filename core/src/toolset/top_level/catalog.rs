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

use rmcp::model::{CallToolResult, Content, JsonObject, Tool};

use crate::auth::AuthSubject;

use super::super::error::ToolSetsError;
use super::super::filter::OutputFilter;
use super::super::traits::{SearchableToolSet, TopLevelTool};

// ---------------------------------------------------------------------------
// CatalogEntry (shared data shape)
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

    /// Run the catalog search under the caller's subject. The read lock is
    /// held only for the synchronous scan and is dropped before this
    /// method returns.
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
                scored.sort_by(|a, b| b.0.cmp(&a.0));
                entries = scored.into_iter().map(|(_, e)| e).collect();
            }
        }

        entries
    }

    /// Normalize a string for fuzzy keyword matching: lowercase and
    /// collapse underscores/hyphens into spaces so "search code",
    /// "search_code", and "search-code" all match each other.
    fn normalize(s: &str) -> String {
        s.to_lowercase().replace(['_', '-'], " ")
    }

    /// Count how many `keywords` appear somewhere in `entry`'s searchable
    /// text (tool name + upstream name + brief description). Used for
    /// ranking search results.
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

    /// Format search results as text grouped by category.
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

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let args = arguments.as_ref();
        let query = args.and_then(|a| a.get("query")).and_then(|v| v.as_str());
        let category = args
            .and_then(|a| a.get("category"))
            .and_then(|v| v.as_str());
        let results = self.execute_search(subject, query, category);
        let text = Self::format_results(&results);
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

    /// Locate a single catalog entry by prefixed name, filtered by the
    /// subject's visibility.
    fn execute_describe(&self, subject: &AuthSubject, prefixed_name: &str) -> Option<CatalogEntry> {
        let sets = self.sets.read().expect("toolset lock poisoned");
        visible_entries(subject, &sets)
            .into_iter()
            .find(|e| e.prefixed_name == prefixed_name)
    }

    /// Format a catalog entry as a detailed markdown description.
    fn format_entry(entry: &CatalogEntry) -> String {
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
        let text = match self.execute_describe(subject, tool_name) {
            Some(entry) => Self::format_entry(&entry),
            None => format!("Tool not found: {tool_name}"),
        };
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}

// ---------------------------------------------------------------------------
// call_tool
// ---------------------------------------------------------------------------

/// The call-a-prefixed-tool meta-tool. Dispatches into the visible +
/// executable `SearchableToolSet` for the caller and applies an optional
/// output filter.
pub struct CallCatalogTool {
    sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
}

impl CallCatalogTool {
    pub fn new(sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>) -> Self {
        Self { sets }
    }

    /// Look up the toolset backing `prefixed_name`, filtered by the
    /// subject's visibility. Returns the Arc'd toolset, the inner tool
    /// name with the prefix stripped, and the tool-specific default
    /// output filter. Returns `None` if the tool is not known or hidden
    /// from `subject`.
    fn find_set(
        &self,
        subject: &AuthSubject,
        prefixed_name: &str,
    ) -> Option<(Arc<dyn SearchableToolSet>, String, Option<OutputFilter>)> {
        let sets = self.sets.read().expect("toolset lock poisoned");
        for set in sets.iter() {
            if !set.is_visible(subject) {
                continue;
            }
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
        let output_filter: Option<OutputFilter> = args
            .remove("output_filter")
            .and_then(|v| serde_json::from_value(v).ok());

        let (set, name, tool_default_filter) = self
            .find_set(subject, &tool_name)
            .ok_or_else(|| ToolSetsError::ToolNotFound(tool_name.clone()))?;

        if !set.can_execute(subject) {
            return Err(ToolSetsError::Unauthorized);
        }

        let result = set.call(&name, inner_args).await;
        let filter = output_filter
            .or(tool_default_filter)
            .unwrap_or_else(OutputFilter::global_default);
        result.and_then(|r| filter.apply(r))
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Enumerate every [`CatalogEntry`] exposed by the toolsets that
/// `subject` can see. Toolsets whose `is_visible(subject)` returns
/// `false` are skipped entirely.
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
                default_output_filter: tool.default_output_filter.clone(),
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
    fn normalize_replaces_underscores_and_hyphens() {
        assert_eq!(SearchCatalog::normalize("search_code"), "search code");
        assert_eq!(SearchCatalog::normalize("list-pipelines"), "list pipelines");
        assert_eq!(SearchCatalog::normalize("Search_Code"), "search code");
        assert_eq!(SearchCatalog::normalize("no changes"), "no changes");
    }
}
