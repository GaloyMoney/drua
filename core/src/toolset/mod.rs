pub mod code_assistant;
pub mod concourse;
mod config;
mod error;
mod traits;
mod upstream;

pub use code_assistant::CodeAssistantToolSet;
pub use concourse::ConcourseToolSet;
pub use config::*;
pub use error::*;
pub use traits::*;
pub use upstream::*;

use std::sync::Arc;

use crate::audit::{Audit, InteractionOutcome};
use crate::auth::AuthContext;
use crate::code_assistant::CodeAssistant;
use rmcp::model::{CallToolResult, JsonObject, Tool};

pub struct CatalogEntry {
    pub prefixed_name: String,
    pub upstream_name: String,
    pub tool_name: String,
    pub category: String,
    pub brief_description: String,
    pub full_tool: Tool,
}

/// Shared catalog of tool sets. Use [`Catalog::with_auth`] to create a
/// request-scoped handle that records audit entries automatically.
pub struct Catalog {
    sets: Arc<Vec<Box<dyn ToolSet>>>,
    audit: Option<Arc<Audit>>,
    auth: Option<AuthContext>,
}

impl Catalog {
    /// Create a request-scoped catalog that records audit entries under the
    /// given [`AuthContext`]. Cheap — only clones `Arc`s.
    pub fn with_auth(&self, auth: &AuthContext) -> Self {
        Self {
            sets: Arc::clone(&self.sets),
            audit: self.audit.clone(),
            auth: Some(auth.clone()),
        }
    }

    pub fn instructions(&self) -> String {
        let mut lines = vec![
            "Tools from upstream services are available via progressive disclosure:".to_string(),
            "1. search_tools — discover tools by keyword or category".to_string(),
            "2. describe_tool — get full parameter schema before calling".to_string(),
            "3. call_tool — execute with proper arguments".to_string(),
            String::new(),
            "Available toolsets:".to_string(),
        ];
        for set in self.sets.iter() {
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

    pub fn entries(&self) -> Vec<CatalogEntry> {
        let mut entries = Vec::new();
        for set in self.sets.iter() {
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

    fn find_set<'a>(&'a self, prefixed_name: &'a str) -> Option<(&'a dyn ToolSet, &'a str)> {
        for set in self.sets.iter() {
            let prefix = format!("{}_", set.prefix());
            if let Some(tool_name) = prefixed_name.strip_prefix(&prefix) {
                if set.tools().iter().any(|t| t.name == tool_name) {
                    return Some((set.as_ref(), tool_name));
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
        let (set, tool_name) = self
            .find_set(prefixed_name)
            .ok_or_else(|| ToolSetsError::ToolNotFound(prefixed_name.to_string()))?;

        let start = std::time::Instant::now();
        let result = set.call(tool_name, arguments.clone()).await;
        let duration_ms = start.elapsed().as_millis() as u64;

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

pub struct ToolSets {
    catalog: Catalog,
}

impl ToolSets {
    #[tracing::instrument(name = "toolset.init", skip_all)]
    pub async fn init(
        config: ToolSetsConfig,
        code_assistant: Option<Arc<CodeAssistant>>,
        audit: Option<Arc<Audit>>,
    ) -> Result<Self, ToolSetsError> {
        let mut sets: Vec<Box<dyn ToolSet>> = Vec::new();

        for upstream in &config.mcp_upstreams {
            if upstream.auth_header.is_empty() {
                tracing::warn!(name = %upstream.name, "Skipping upstream — no auth header set");
                continue;
            }
            match UpstreamToolSet::init(upstream).await {
                Ok(ts) => {
                    tracing::info!(
                        name = %upstream.name,
                        prefix = ts.prefix(),
                        tools = ts.tools().len(),
                        "MCP upstream toolset initialized"
                    );
                    sets.push(Box::new(ts));
                }
                Err(e) => {
                    tracing::warn!(name = %upstream.name, error = %e, "Failed to initialize MCP upstream, skipping");
                }
            }
        }

        if config.concourse.enabled
            && !config.concourse.url.is_empty()
            && !config.concourse.username.is_empty()
        {
            let client = concourse_client::ConcourseClient::new(
                &config.concourse.url,
                config.concourse.team.clone(),
                config.concourse.username.clone(),
                config.concourse.password.clone(),
            )?;
            sets.push(Box::new(ConcourseToolSet::new(client)));
            tracing::info!(url = %config.concourse.url, "Concourse toolset initialized");
        }

        if let Some(ca) = code_assistant {
            sets.push(Box::new(CodeAssistantToolSet::new(ca)));
            tracing::info!("Code assistant toolset initialized");
        }

        Ok(Self {
            catalog: Catalog {
                sets: Arc::new(sets),
                audit,
                auth: None,
            },
        })
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
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
        ) -> Result<CallToolResult, ToolSetsError> {
            unimplemented!()
        }
    }

    fn test_catalog(sets: Vec<Box<dyn ToolSet>>) -> Catalog {
        Catalog {
            sets: Arc::new(sets),
            audit: None,
            auth: None,
        }
    }

    #[tokio::test]
    async fn search_ranks_by_keyword_hits() {
        let catalog = test_catalog(vec![Box::new(StubToolSet::with_tools(vec![
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
        let catalog = test_catalog(vec![Box::new(StubToolSet::with_tool(
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
        let catalog = test_catalog(vec![Box::new(StubToolSet::with_tool(
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
        let catalog = test_catalog(vec![Box::new(StubToolSet::with_tools(vec![
            ("list_pipelines", "List CI pipelines"),
            ("get_build_log", "Get build output"),
        ]))]);

        let results = catalog.search(Some("pipeline"), None).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].tool_name, "list_pipelines");
    }

    #[tokio::test]
    async fn search_no_match_returns_empty() {
        let catalog = test_catalog(vec![Box::new(StubToolSet::with_tool(
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
}
