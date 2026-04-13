mod builtins;
mod catalog;
pub mod concourse;
mod config;
mod error;
mod filter;
mod traits;
mod upstream;

pub use builtins::{CallCatalogTool, DescribeCatalogTool, SearchCatalog};
pub use catalog::{Catalog, CatalogEntry};
pub use concourse::ConcourseToolSet;
pub use config::*;
pub use error::*;
pub use filter::OutputFilter;
pub use traits::*;
pub use upstream::*;

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use rmcp::model::{CallToolResult, JsonObject};

use crate::audit::{Audit, InteractionOutcome};
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

    /// Register a toolset after initialization (e.g. for breaking circular deps).
    pub fn register(&self, toolset: impl SearchableToolSet + 'static) {
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

    /// Build a catalog handle that shares the toolset registry.
    pub fn catalog(&self) -> Catalog {
        Catalog::new(Arc::clone(&self.sets))
    }

    /// Iterator over the registered top-level tools — used by the MCP server
    /// to populate its `list_tools` response.
    pub fn top_level_tools(&self) -> impl Iterator<Item = &Arc<dyn TopLevelTool>> {
        self.top_level.values()
    }

    /// Look up and execute a top-level tool by name. Performs the same
    /// boilerplate as [`dispatch_tool_call`] — scope check + dispatch + audit
    /// — so the MCP server's `call_tool` RPC can delegate straight here.
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

        let scopes: Vec<&str> = subject.scopes().iter().map(String::as_str).collect();
        if !tool.is_authorized(&scopes, arguments.as_ref()) {
            return Err(ToolSetsError::ToolNotFound(name.to_string()));
        }

        let start = std::time::Instant::now();
        let result = tool.call(arguments.clone()).await;
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

    /// Execute a tool with the global default output filter.
    pub async fn call(
        &self,
        subject: &AuthSubject,
        prefixed_name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        self.call_with_filter(subject, prefixed_name, arguments, None)
            .await
    }

    /// Execute a tool, performing scope checks against `subject` and recording
    /// an audit entry. The first matching toolset that the subject is
    /// authorized to access is used.
    pub async fn call_with_filter(
        &self,
        subject: &AuthSubject,
        prefixed_name: &str,
        arguments: Option<JsonObject>,
        output_filter: Option<OutputFilter>,
    ) -> Result<CallToolResult, ToolSetsError> {
        dispatch_tool_call(
            &self.sets,
            &self.audit,
            subject,
            prefixed_name,
            arguments,
            output_filter,
        )
        .await
    }
}

/// Shared dispatch helper used by both [`ToolSets::call_with_filter`] and the
/// [`CallTool`] top-level tool. Performs: lookup, scope check, dispatch,
/// output filter, audit recording.
pub(super) async fn dispatch_tool_call(
    sets: &Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
    audit: &Option<Arc<Audit>>,
    subject: &AuthSubject,
    prefixed_name: &str,
    arguments: Option<JsonObject>,
    output_filter: Option<OutputFilter>,
) -> Result<CallToolResult, ToolSetsError> {
    let catalog = Catalog::new(Arc::clone(sets));
    let (set, tool_name, tool_default_filter) = catalog
        .find_set(prefixed_name)
        .ok_or_else(|| ToolSetsError::ToolNotFound(prefixed_name.to_string()))?;

    let scopes: Vec<&str> = subject.scopes().iter().map(String::as_str).collect();
    if !has_scopes(set.required_scopes(), &scopes) {
        return Err(ToolSetsError::ToolNotFound(prefixed_name.to_string()));
    }

    let start = std::time::Instant::now();
    let result = set.call(&tool_name, arguments.clone(), Some(subject)).await;
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
    if let Some(audit) = audit {
        if let Err(e) = audit
            .record_mcp_call(
                subject,
                prefixed_name,
                Some(&serde_json::json!({
                    "tool_name": prefixed_name,
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

fn has_scopes(required: &[&str], available: &[&str]) -> bool {
    if required.is_empty() {
        return true;
    }
    required.iter().all(|scope| available.contains(scope))
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
