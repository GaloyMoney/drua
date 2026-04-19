mod config;
mod error;
mod filter;
pub mod searchable;
pub mod top_level;
mod traits;

pub use config::*;
pub use error::*;
pub use filter::OutputFilter;
pub use searchable::*;
pub use top_level::{
    AdminAgentAttachSandbox, AdminAgentCreate, AdminAgentDetachSandbox, AdminAllLogs,
    AdminCreateSandbox, AdminGetSandbox, AdminInspectSandbox, AdminListAgents, AdminListSandboxes,
    AdminWorkspaceCreate, AdminWorkspaceList, Bash, CallCatalogTool, DescribeCatalogTool, GlobTool,
    Grep, Ls, Read, SearchCatalog, TextEditor, WhoAmI, WorkspaceAgentAttachSandbox,
    WorkspaceAgentCreate, WorkspaceAgentDetachSandbox, WorkspaceCreateSandbox, WorkspaceGetSandbox,
    WorkspaceInspectSandbox, WorkspaceListAgents, WorkspaceListSandboxes, WorkspaceLog,
};
pub use traits::*;

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use rmcp::model::{CallToolResult, JsonObject};

use crate::audit::Audit;
use crate::auth::AuthSubject;

pub struct ToolSets {
    sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
    top_level: RwLock<HashMap<String, Arc<dyn TopLevelTool>>>,
    audit: Option<Arc<Audit>>,
    init_errors: Vec<(String, String)>,
}

impl ToolSets {
    pub async fn init(config: ToolSetsConfig) -> Result<Self, ToolSetsError> {
        let mut sets: Vec<Arc<dyn SearchableToolSet>> = Vec::new();
        let mut init_errors: Vec<(String, String)> = Vec::new();

        for upstream in &config.mcp_upstreams {
            match UpstreamToolSet::init(upstream).await {
                Ok(ts) => {
                    sets.push(Arc::new(ts));
                }
                Err(e) => {
                    init_errors.push((upstream.name.clone(), e.to_string()));
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

        let search = Arc::new(SearchCatalog::new(Arc::clone(&sets)));
        let describe = Arc::new(DescribeCatalogTool::new(Arc::clone(&sets)));
        let call = Arc::new(CallCatalogTool::new(Arc::clone(&sets)));
        let whoami = Arc::new(WhoAmI::new());

        let mut top_level = HashMap::new();
        top_level.insert(search.name().to_string(), search as Arc<dyn TopLevelTool>);
        top_level.insert(
            describe.name().to_string(),
            describe as Arc<dyn TopLevelTool>,
        );
        top_level.insert(call.name().to_string(), call as Arc<dyn TopLevelTool>);
        top_level.insert(whoami.name().to_string(), whoami as Arc<dyn TopLevelTool>);

        Ok(Self {
            sets,
            top_level: RwLock::new(top_level),
            audit: None,
            init_errors,
        })
    }

    /// Log upstream init results. Must be called from OUTSIDE
    /// `ToolSets::init` — rmcp's `serve_inner` captures
    /// `tracing::Span::current()` and instruments a long-lived background
    /// task with it. That task holds the span open for the MCP
    /// connection's lifetime, so `tracing-opentelemetry` never exports it
    /// (spans export only on close, i.e. when all handles are dropped).
    #[tracing::instrument(name = "domain.toolset.init_summary", skip_all)]
    pub fn log_init_summary(&self) {
        let sets = self.sets.read().expect("toolset lock poisoned");
        for set in sets.iter() {
            tracing::info!(
                upstream.name = set.name(),
                upstream.prefix = set.prefix(),
                upstream.tools = set.tools().len(),
                upstream.category = set.category(),
                "MCP upstream initialized"
            );
        }
        for (name, error) in &self.init_errors {
            tracing::error!(
                upstream.name = %name,
                error = %error,
                "Failed to initialize MCP upstream"
            );
        }
    }

    /// Wire the audit service so tool calls are automatically recorded.
    /// Optional — when `None` (e.g. in tests) audit is silently skipped.
    pub fn set_audit(&mut self, audit: Arc<Audit>) {
        self.audit = Some(audit);
    }

    /// Register a top-level tool. Uses interior mutability so tools can be
    /// registered even after the `ToolSets` is wrapped in an `Arc`.
    pub fn register_top_level(&self, tool: impl TopLevelTool + 'static) {
        let tool: Arc<dyn TopLevelTool> = Arc::new(tool);
        let name = tool.name().to_string();
        tracing::info!(name = %name, "Registered top-level tool");
        self.top_level
            .write()
            .expect("top_level lock poisoned")
            .insert(name, tool);
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
    pub fn mcp_gateway_info(&self) -> String {
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
    pub fn top_level_tools(
        &self,
        subject: &AuthSubject,
    ) -> impl Iterator<Item = Arc<dyn TopLevelTool>> {
        let map = self.top_level.read().expect("top_level lock poisoned");
        map.values()
            .filter(|t| t.is_visible(subject))
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
    }

    /// Look up and execute a top-level tool by name. Records an audit
    /// entry when an [`Audit`] instance has been wired via [`set_audit`].
    pub async fn call_top_level_tool(
        &self,
        subject: &AuthSubject,
        name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        use es_entity::context::{EventContext, WithEventContext};

        let tool = {
            let map = self.top_level.read().expect("top_level lock poisoned");
            Arc::clone(
                map.get(name)
                    .ok_or_else(|| ToolSetsError::ToolNotFound(name.to_string()))?,
            )
        };

        let seed = {
            let ctx = EventContext::current();
            ctx.data()
        };

        let audit = self.audit.clone();

        async move {
            Audit::record_subject(subject);
            Audit::record_action(name);
            let args_value = arguments
                .as_ref()
                .map(|a| serde_json::Value::Object(a.clone()));
            Audit::record_metadata(serde_json::json!({
                "tool_name": name,
                "arguments": args_value,
            }));

            let start = std::time::Instant::now();
            let result = tool.call(subject, arguments).await;
            Audit::record_duration(start);

            match &result {
                Ok(r) => {
                    Audit::record_tokens(estimate_tokens(r));
                    Audit::record_success();
                }
                Err(e) => {
                    Audit::record_error(e.to_string());
                }
            }

            if let Some(audit) = &audit {
                audit.record_from_context();
            }

            result
        }
        .with_event_context(seed)
        .await
    }
}

/// Estimate token count from a [`CallToolResult`]'s text content (~4 chars
/// per token).
pub fn estimate_tokens(result: &CallToolResult) -> u64 {
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
