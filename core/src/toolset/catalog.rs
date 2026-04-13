use std::sync::{Arc, RwLock};

use rmcp::model::{CallToolResult, JsonObject, Tool};

use crate::audit::{Audit, InteractionOutcome};
use crate::auth::AuthSubject;

use super::error::ToolSetsError;
use super::filter::OutputFilter;
use super::traits::ToolSet;

pub struct CatalogEntry {
    pub prefixed_name: String,
    pub upstream_name: String,
    pub tool_name: String,
    pub category: String,
    pub brief_description: String,
    pub full_tool: Tool,
    pub default_output_filter: Option<OutputFilter>,
}

/// Shared catalog of tool sets. Use [`Catalog::with_auth`] to create a
/// request-scoped handle that records audit entries automatically.
///
/// Toolsets are stored as `Arc<dyn ToolSet>` so they can be cloned out
/// from behind the `RwLock` without holding the lock across `.await`.
pub struct Catalog {
    pub(super) sets: Arc<RwLock<Vec<Arc<dyn ToolSet>>>>,
    pub(super) audit: Option<Arc<Audit>>,
    pub(super) auth: Option<AuthSubject>,
}

impl Catalog {
    /// Create a request-scoped catalog that records audit entries under the
    /// given [`AuthSubject`]. Cheap — only clones `Arc`s.
    pub fn with_auth(&self, auth: &AuthSubject) -> Self {
        Self {
            sets: Arc::clone(&self.sets),
            audit: self.audit.clone(),
            auth: Some(auth.clone()),
        }
    }

    /// Acquire a read lock on the toolset registry.
    fn read_sets(&self) -> std::sync::RwLockReadGuard<'_, Vec<Arc<dyn ToolSet>>> {
        self.sets.read().expect("toolset lock poisoned")
    }

    /// Format search results as text grouped by category.
    pub fn format_search_results(results: &[CatalogEntry]) -> String {
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
    pub fn format_describe(entry: &CatalogEntry) -> String {
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

    pub fn instructions(&self) -> String {
        let sets = self.read_sets();
        let mut lines = vec![
            "Tools from upstream services are available via progressive disclosure:".to_string(),
            "1. search_tools — discover tools by keyword or category".to_string(),
            "2. describe_tool — get full parameter schema before calling".to_string(),
            "3. call_tool — execute with proper arguments".to_string(),
            String::new(),
            "Available toolsets:".to_string(),
        ];
        for set in sets.iter() {
            if !self.caller_has_required_scopes(set.as_ref()) {
                continue;
            }
            let cat = set.category();
            let desc = set.category_description();
            let tool_count = set.tools().len();
            lines.push(format!(
                "  {} ({}, {} tools) — {}",
                set.name(),
                cat,
                tool_count,
                desc,
            ));
        }
        lines.join("\n")
    }

    /// Check whether the current auth context satisfies a toolset's required scopes.
    fn caller_has_required_scopes(&self, set: &dyn ToolSet) -> bool {
        let required = set.required_scopes();
        if required.is_empty() {
            return true;
        }
        match &self.auth {
            Some(auth) => required.iter().all(|scope| auth.has_scope(scope)),
            None => false,
        }
    }

    pub fn entries(&self) -> Vec<CatalogEntry> {
        let sets = self.read_sets();
        let mut entries = Vec::new();
        for set in sets.iter() {
            if !self.caller_has_required_scopes(set.as_ref()) {
                continue;
            }
            for tool in set.tools() {
                let desc = &tool.description;
                let brief = desc
                    .description
                    .as_ref()
                    .map(|d| first_sentence(d))
                    .unwrap_or_default();
                entries.push(CatalogEntry {
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
        entries
    }

    pub async fn search(&self, query: Option<&str>, category: Option<&str>) -> Vec<CatalogEntry> {
        let start = std::time::Instant::now();
        let mut entries: Vec<CatalogEntry> = self
            .entries()
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
        let duration_ms = start.elapsed().as_millis() as u64;

        self.record_audit(
            "search_tools",
            serde_json::json!({ "query": query, "category": category }),
            InteractionOutcome::Success,
            Some(duration_ms),
            None,
        )
        .await;

        entries
    }

    pub async fn describe(&self, prefixed_name: &str) -> Option<CatalogEntry> {
        let start = std::time::Instant::now();
        let result = self
            .entries()
            .into_iter()
            .find(|e| e.prefixed_name == prefixed_name);
        let duration_ms = start.elapsed().as_millis() as u64;

        let outcome = if result.is_some() {
            InteractionOutcome::Success
        } else {
            InteractionOutcome::Error {
                message: format!("tool not found: {prefixed_name}"),
            }
        };
        self.record_audit(
            "describe_tool",
            serde_json::json!({ "tool_name": prefixed_name }),
            outcome,
            Some(duration_ms),
            None,
        )
        .await;

        result
    }

    /// Find the toolset, stripped tool name, and default output filter for a
    /// prefixed tool name. Returns an `Arc` clone so the lock is not held
    /// across `.await`.
    fn find_set(
        &self,
        prefixed_name: &str,
    ) -> Option<(Arc<dyn ToolSet>, String, Option<OutputFilter>)> {
        let sets = self.read_sets();
        for set in sets.iter() {
            if !self.caller_has_required_scopes(set.as_ref()) {
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

    pub async fn call(
        &self,
        prefixed_name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        self.call_with_filter(prefixed_name, arguments, None).await
    }

    pub async fn call_with_filter(
        &self,
        prefixed_name: &str,
        arguments: Option<JsonObject>,
        output_filter: Option<OutputFilter>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let (set, tool_name, tool_default_filter) = self
            .find_set(prefixed_name)
            .ok_or_else(|| ToolSetsError::ToolNotFound(prefixed_name.to_string()))?;

        let start = std::time::Instant::now();
        let result = set
            .call(&tool_name, arguments.clone(), self.auth.as_ref())
            .await;
        let duration_ms = start.elapsed().as_millis() as u64;

        // Priority: caller-provided > tool-specific default > global default
        let filter = output_filter
            .or(tool_default_filter)
            .unwrap_or_else(OutputFilter::global_default);
        let result = result.and_then(|r| filter.apply(r));

        let (outcome, tokens_returned) = match &result {
            Ok(call_result) => {
                let tokens = estimate_tokens(call_result);
                (InteractionOutcome::Success, Some(tokens))
            }
            Err(e) => (
                InteractionOutcome::Error {
                    message: e.to_string(),
                },
                None,
            ),
        };
        let args_value = arguments.map(serde_json::Value::Object);
        self.record_audit(
            prefixed_name,
            serde_json::json!({
                "tool_name": prefixed_name,
                "arguments": args_value,
            }),
            outcome,
            Some(duration_ms),
            tokens_returned,
        )
        .await;

        result
    }

    /// Fire-and-forget audit recording. Only records if both audit service
    /// and auth context are available (i.e. this is a request-scoped catalog).
    async fn record_audit(
        &self,
        tool_name: &str,
        metadata: serde_json::Value,
        outcome: InteractionOutcome,
        duration_ms: Option<u64>,
        tokens_returned: Option<u64>,
    ) {
        if let (Some(audit), Some(auth)) = (&self.audit, &self.auth) {
            if let Err(e) = audit
                .record_mcp_call(
                    auth,
                    tool_name,
                    Some(&metadata),
                    outcome,
                    duration_ms,
                    tokens_returned,
                )
                .await
            {
                tracing::warn!(error = %e, "Failed to record audit entry");
            }
        }
    }
}

/// Estimate token count from a CallToolResult's text content (~4 chars per token).
fn estimate_tokens(result: &CallToolResult) -> u64 {
    let total_chars: usize = result
        .content
        .iter()
        .map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => t.text.len(),
            _ => 0,
        })
        .sum();
    (total_chars / 4).max(1) as u64
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

#[cfg(test)]
mod tests {
    use super::super::traits::ToolSetEntry;
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
    impl ToolSet for StubToolSet {
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
            _auth: Option<&AuthSubject>,
        ) -> Result<CallToolResult, ToolSetsError> {
            unimplemented!()
        }
    }

    fn test_catalog(sets: Vec<Arc<dyn ToolSet>>) -> Catalog {
        Catalog {
            sets: Arc::new(RwLock::new(sets)),
            audit: None,
            auth: None,
        }
    }

    #[tokio::test]
    async fn search_ranks_by_keyword_hits() {
        let catalog = test_catalog(vec![Arc::new(StubToolSet::with_tools(vec![
            ("get_pipeline_status", "Get pipeline build status"),
            ("list_pipelines", "List CI pipelines"),
            ("search_code", "Semantic search over indexed codebases"),
        ]))]);

        // "pipeline status" → get_pipeline_status matches both keywords (score 2),
        // list_pipelines matches only "pipeline" (score 1)
        let results = catalog.search(Some("pipeline status"), None).await;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].tool_name, "get_pipeline_status");
        assert_eq!(results[1].tool_name, "list_pipelines");
    }

    #[tokio::test]
    async fn search_individual_keywords_match_across_name() {
        let catalog = test_catalog(vec![Arc::new(StubToolSet::with_tool(
            "get_pipeline_status",
            "Returns the current status of a CI pipeline",
        ))]);

        // "pipeline status" as keywords each match inside "get_pipeline_status"
        let results = catalog.search(Some("pipeline status"), None).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].tool_name, "get_pipeline_status");
    }

    #[tokio::test]
    async fn search_normalizes_underscores_and_hyphens() {
        let catalog = test_catalog(vec![Arc::new(StubToolSet::with_tool(
            "search_code",
            "Semantic search over indexed codebases",
        ))]);

        // All three forms match
        for query in ["search code", "search_code", "search-code"] {
            let results = catalog.search(Some(query), None).await;
            assert_eq!(results.len(), 1, "query '{query}' should match");
            assert_eq!(results[0].tool_name, "search_code");
        }
    }

    #[tokio::test]
    async fn search_single_keyword_matches() {
        let catalog = test_catalog(vec![Arc::new(StubToolSet::with_tools(vec![
            ("list_pipelines", "List CI pipelines"),
            ("get_build_log", "Get build output"),
        ]))]);

        let results = catalog.search(Some("pipeline"), None).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].tool_name, "list_pipelines");
    }

    #[tokio::test]
    async fn search_no_match_returns_empty() {
        let catalog = test_catalog(vec![Arc::new(StubToolSet::with_tool(
            "search_code",
            "Semantic search over indexed codebases",
        ))]);

        let results = catalog.search(Some("nonexistent"), None).await;
        assert!(results.is_empty());
    }

    #[test]
    fn normalize_replaces_underscores_and_hyphens() {
        assert_eq!(normalize("search_code"), "search code");
        assert_eq!(normalize("list-pipelines"), "list pipelines");
        assert_eq!(normalize("Search_Code"), "search code");
        assert_eq!(normalize("no changes"), "no changes");
    }

    // ── Scope-filtering tests ─────────────────────────────────────────

    struct ScopedStubToolSet {
        entries: Vec<ToolSetEntry>,
        scopes: Vec<&'static str>,
    }

    impl ScopedStubToolSet {
        fn new(name: &str, description: &str, scopes: Vec<&'static str>) -> Self {
            let tool = Tool::new(
                name.to_string(),
                description.to_string(),
                JsonObject::default(),
            );
            Self {
                entries: vec![ToolSetEntry {
                    name: name.to_string(),
                    description: tool,
                    default_output_filter: None,
                }],
                scopes,
            }
        }
    }

    #[async_trait::async_trait]
    impl ToolSet for ScopedStubToolSet {
        fn name(&self) -> &str {
            "scoped"
        }
        fn category(&self) -> &str {
            "admin"
        }
        fn category_description(&self) -> &str {
            "Admin tools"
        }
        fn tools(&self) -> &[ToolSetEntry] {
            &self.entries
        }
        fn required_scopes(&self) -> &[&str] {
            &self.scopes
        }
        async fn call(
            &self,
            _tool_name: &str,
            _arguments: Option<JsonObject>,
            _auth: Option<&AuthSubject>,
        ) -> Result<CallToolResult, ToolSetsError> {
            Ok(CallToolResult::success(vec![]))
        }
    }

    use crate::primitives::{McpCredsId, UserId};

    fn admin_auth() -> AuthSubject {
        AuthSubject::ExportedAgent(UserId::new(), McpCredsId::new(), vec!["admin".to_string()])
    }

    fn unprivileged_auth() -> AuthSubject {
        AuthSubject::ExportedAgent(UserId::new(), McpCredsId::new(), vec![])
    }

    #[tokio::test]
    async fn scoped_toolset_visible_with_matching_scope() {
        let catalog = test_catalog(vec![Arc::new(ScopedStubToolSet::new(
            "list_workspaces",
            "List all workspaces",
            vec!["admin"],
        ))]);
        let catalog = catalog.with_auth(&admin_auth());

        let entries = catalog.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].tool_name, "list_workspaces");
    }

    #[tokio::test]
    async fn scoped_toolset_hidden_without_scope() {
        let catalog = test_catalog(vec![Arc::new(ScopedStubToolSet::new(
            "list_workspaces",
            "List all workspaces",
            vec!["admin"],
        ))]);
        let catalog = catalog.with_auth(&unprivileged_auth());

        let entries = catalog.entries();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn scoped_toolset_hidden_without_auth() {
        let catalog = test_catalog(vec![Arc::new(ScopedStubToolSet::new(
            "list_workspaces",
            "List all workspaces",
            vec!["admin"],
        ))]);

        // No auth at all
        let entries = catalog.entries();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn search_respects_scopes() {
        let catalog = test_catalog(vec![
            Arc::new(StubToolSet::with_tool(
                "list_pipelines",
                "List CI pipelines",
            )),
            Arc::new(ScopedStubToolSet::new(
                "list_workspaces",
                "List all workspaces",
                vec!["admin"],
            )),
        ]);

        // Unprivileged: only sees pipelines
        let catalog_unpriv = catalog.with_auth(&unprivileged_auth());
        let results = catalog_unpriv.search(Some("list"), None).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].tool_name, "list_pipelines");

        // Admin: sees both
        let catalog_admin = catalog.with_auth(&admin_auth());
        let results = catalog_admin.search(Some("list"), None).await;
        assert_eq!(results.len(), 2);
    }
}
