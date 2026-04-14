mod builtins;
pub mod code_assistant;
pub mod concourse;
mod config;
mod error;
mod filter;
mod traits;
mod upstream;

pub use builtins::{CallCatalogTool, DescribeCatalogTool, SearchCatalog};
pub use code_assistant::CodeAssistantToolSet;
pub use concourse::ConcourseToolSet;
pub use config::*;
pub use error::*;
pub use filter::OutputFilter;
pub use traits::*;
pub use upstream::*;

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use rmcp::model::{CallToolResult, JsonObject};

use crate::audit::primitives::InteractionOutcome;
use crate::audit::Audit;
use crate::auth::AuthSubject;

pub struct ToolSets {
    sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
    audit: Option<Arc<Audit>>,
    top_level: HashMap<String, Arc<dyn TopLevelTool>>,
}

impl ToolSets {
    #[tracing::instrument(name = "toolset.init", skip_all)]
    pub async fn init(
        config: ToolSetsConfig,
        audit: Option<Arc<Audit>>,
    ) -> Result<Self, ToolSetsError> {
        let mut sets: Vec<Arc<dyn SearchableToolSet>> = Vec::new();

        for upstream in &config.mcp_upstreams {
            match UpstreamToolSet::init(upstream).await {
                Ok(ts) => {
                    tracing::info!(
                        name = %upstream.name,
                        prefix = ts.prefix(),
                        tools = ts.tools().len(),
                        "MCP upstream toolset initialized"
                    );
                    sets.push(Arc::new(ts));
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
            sets.push(Arc::new(ConcourseToolSet::new(client)));
            tracing::info!(url = %config.concourse.url, "Concourse toolset initialized");
        }

        let sets = Arc::new(RwLock::new(sets));

        let mut top_level: HashMap<String, Arc<dyn TopLevelTool>> = HashMap::new();
        let search = Arc::new(SearchCatalog::new(Arc::clone(&sets)));
        let describe = Arc::new(DescribeCatalogTool::new(Arc::clone(&sets)));
        let call = Arc::new(CallCatalogTool::new(Arc::clone(&sets)));
        top_level.insert(search.name().to_string(), search);
        top_level.insert(describe.name().to_string(), describe);
        top_level.insert(call.name().to_string(), call);

        Ok(Self {
            sets,
            audit,
            top_level,
        })
    }

    /// Register a top-level tool. Intended to be called during init before the
    /// `ToolSets` value is wrapped in an `Arc` and shared.
    pub fn register_top_level(&mut self, tool: impl TopLevelTool + 'static) {
        let tool: Arc<dyn TopLevelTool> = Arc::new(tool);
        let name = tool.name().to_string();
        tracing::info!(name = %name, "Registered top-level tool");
        self.top_level.insert(name, tool);
    }

    pub fn register_searchable(&self, toolset: impl SearchableToolSet + 'static) {
        let toolset: Arc<dyn SearchableToolSet> = Arc::new(toolset);
        let mut sets = self.sets.write().expect("toolset lock poisoned");
        tracing::info!(
            name = toolset.name(),
            category = toolset.category(),
            tools = toolset.tools().len(),
            "Late-registered toolset"
        );
        sets.push(toolset);
    }

    /// Human-readable summary of available toolsets — used as the MCP
    /// server's `instructions` payload so clients know how to discover and
    /// call upstream tools.
    pub fn instructions(&self) -> String {
        let sets = self.sets.read().expect("toolset lock poisoned");
        let mut lines = vec![
            "Tools from upstream services are available via progressive disclosure:".to_string(),
            "1. search_tools — discover tools by keyword or category".to_string(),
            "2. describe_tool — get full parameter schema before calling".to_string(),
            "3. call_tool — execute with proper arguments".to_string(),
            String::new(),
            "Available toolsets:".to_string(),
        ];
        for set in sets.iter() {
            lines.push(format!(
                "  {} ({}, {} tools) — {}",
                set.name(),
                set.category(),
                set.tools().len(),
                set.category_description(),
            ));
        }
        lines.join("\n")
    }

    /// Top-level tools visible to the given `subject`. A tool is included
    /// iff its [`TopLevelTool::is_visible`] returns `true`. Used to populate
    /// prompt `tools` arrays and the MCP `list_tools` response.
    pub fn top_level_tools<'a>(
        &'a self,
        subject: &'a AuthSubject,
    ) -> impl Iterator<Item = &'a Arc<dyn TopLevelTool>> + 'a {
        self.top_level
            .values()
            .filter(move |t| t.is_visible(subject))
    }

    /// Look up and execute a top-level tool by name. Runs
    /// [`TopLevelTool::can_execute`] + dispatch + audit so the MCP server's
    /// `call_tool` RPC can delegate straight here.
    pub async fn call_top_level_tool(
        &self,
        subject: &AuthSubject,
        name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let tool = self
            .top_level
            .get(name)
            .ok_or_else(|| ToolSetsError::ToolNotFound(name.to_string()))?;

        if !tool.can_execute(subject) {
            return Err(ToolSetsError::Unauthorized);
        }

        let start = std::time::Instant::now();
        let result = tool.call(subject, arguments.clone()).await;
        let duration_ms = start.elapsed().as_millis() as u64;

        let (outcome, tokens_returned) = match &result {
            Ok(r) => (InteractionOutcome::Success, Some(estimate_tokens(r))),
            Err(e) => (
                InteractionOutcome::Error {
                    message: e.to_string(),
                },
                None,
            ),
        };
        let args_value = arguments.map(serde_json::Value::Object);
        if let Some(audit) = &self.audit {
            if let Err(e) = audit
                .record_mcp_call(
                    subject,
                    name,
                    Some(&serde_json::json!({
                        "tool_name": name,
                        "arguments": args_value,
                    })),
                    outcome,
                    Some(duration_ms),
                    tokens_returned,
                )
                .await
            {
                tracing::warn!(error = %e, "Failed to record audit entry");
            }
        }

        result
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
