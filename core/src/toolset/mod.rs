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
    Bash, CallCatalogTool, DescribeCatalogTool, GlobTool, Grep, Ls, Read, SearchCatalog,
    TextEditor, WhoAmI, WorkspaceAgentAttachSandbox, WorkspaceAgentCreate,
    WorkspaceAgentDetachSandbox, WorkspaceCreateSandbox, WorkspaceGetSandbox,
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

    /// Atomically replace every tunnel toolset currently registered under
    /// `deployment_id` with `new_sets`. Done under a single write lock, which:
    ///
    /// 1. closes the overlap window between a new connector's registration
    ///    and the evicted connector's cleanup — without this, first-match
    ///    routing in the append-only vec would temporarily send calls to
    ///    the dying connector;
    /// 2. prevents duplicate toolset names for a deployment, so
    ///    `describe_tool` / `search_tools` never show two copies of the
    ///    same entry during takeover.
    ///
    /// The evicted loop's later [`Self::unregister_searchable_by_session`]
    /// call will find nothing matching its own session_id and no-op,
    /// leaving the freshly-registered entries intact.
    pub fn replace_tunnel_toolsets(
        &self,
        deployment_id: &str,
        new_sets: Vec<Arc<dyn SearchableToolSet>>,
    ) {
        let mut sets = self.sets.write().expect("toolset lock poisoned");
        let before = sets.len();
        sets.retain(|s| match s.scope() {
            Some(ToolSetScope::Tunnel {
                deployment_id: d, ..
            }) => d != deployment_id,
            _ => true,
        });
        let removed = before - sets.len();
        let added = new_sets.len();
        sets.extend(new_sets);
        tracing::info!(
            deployment_id = %deployment_id,
            removed,
            added,
            "Replaced tunnel toolsets"
        );
    }

    /// Remove every tunnel-scoped toolset owned by `session_id`. Called
    /// from a WS loop's cleanup path. If the session was already evicted
    /// by a newer connector (which reused the `deployment_id` via
    /// [`Self::replace_tunnel_toolsets`]), this is a no-op — the new
    /// session has a different `session_id` and its entries are not
    /// touched. This is the invariant that makes takeover safe.
    pub fn unregister_searchable_by_session(&self, session_id: uuid::Uuid) {
        let mut sets = self.sets.write().expect("toolset lock poisoned");
        let before = sets.len();
        sets.retain(|s| match s.scope() {
            Some(ToolSetScope::Tunnel {
                session_id: sid, ..
            }) => *sid != session_id,
            _ => true,
        });
        let removed = before - sets.len();
        if removed > 0 {
            tracing::info!(
                session_id = %session_id,
                removed,
                "Unregistered toolsets for session"
            );
        }
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
            Audit::record_entrypoint(format!("mcp: {}", name));
            Audit::record_interaction_type(crate::audit::primitives::InteractionType::McpCall);
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

#[cfg(test)]
impl ToolSets {
    /// Test-only: construct an empty ToolSets without running `init`'s
    /// I/O (upstream MCP dialers, Concourse client). Tests exercising the
    /// catalog's dynamic-registration primitives don't need any of that.
    pub fn empty_for_test() -> Self {
        Self {
            sets: Arc::new(RwLock::new(Vec::new())),
            top_level: RwLock::new(HashMap::new()),
            audit: None,
            init_errors: Vec::new(),
        }
    }

    /// Test-only: snapshot the current set of toolset names, in
    /// registration order. Used to assert takeover semantics without
    /// exposing the internal Vec.
    pub fn toolset_names_for_test(&self) -> Vec<String> {
        self.sets
            .read()
            .expect("toolset lock poisoned")
            .iter()
            .map(|s| s.name().to_string())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::CallToolResult;

    /// A minimal stub to exercise `replace_tunnel_toolsets` /
    /// `unregister_searchable_by_session` without spinning up a real
    /// tunnel. Its `call()` will never be invoked by these tests.
    struct StubToolSet {
        name: String,
        scope: Option<ToolSetScope>,
        tools: Vec<ToolSetEntry>,
    }

    impl StubToolSet {
        fn tunnel(name: &str, deployment_id: &str, session_id: uuid::Uuid) -> Self {
            Self {
                name: name.to_string(),
                scope: Some(ToolSetScope::Tunnel {
                    deployment_id: deployment_id.to_string(),
                    session_id,
                }),
                tools: Vec::new(),
            }
        }

        fn static_(name: &str) -> Self {
            Self {
                name: name.to_string(),
                scope: None,
                tools: Vec::new(),
            }
        }
    }

    #[async_trait::async_trait]
    impl SearchableToolSet for StubToolSet {
        fn name(&self) -> &str {
            &self.name
        }
        fn category(&self) -> &str {
            "stub"
        }
        fn category_description(&self) -> &str {
            "stub"
        }
        fn tools(&self) -> &[ToolSetEntry] {
            &self.tools
        }
        fn scope(&self) -> Option<&ToolSetScope> {
            self.scope.as_ref()
        }
        async fn call(
            &self,
            _subject: &AuthSubject,
            _tool_name: &str,
            _arguments: Option<JsonObject>,
        ) -> Result<CallToolResult, ToolSetsError> {
            unreachable!("stub: call should not be invoked by these tests")
        }
    }

    /// Atomic swap: `replace_tunnel_toolsets` drops every toolset whose
    /// scope is `Tunnel(deployment_id, _)` and appends the new ones —
    /// in one write-lock, so routing never sees the old entries.
    #[test]
    fn replace_tunnel_toolsets_swaps_by_deployment() {
        let toolsets = ToolSets::empty_for_test();
        let old_session = uuid::Uuid::new_v4();
        toolsets.register_searchable(StubToolSet::tunnel("stg-k8s", "staging", old_session));
        toolsets.register_searchable(StubToolSet::tunnel("stg-pg", "staging", old_session));
        toolsets.register_searchable(StubToolSet::static_("concourse"));
        assert_eq!(
            toolsets.toolset_names_for_test(),
            vec!["stg-k8s", "stg-pg", "concourse"]
        );

        let new_session = uuid::Uuid::new_v4();
        let new_sets: Vec<Arc<dyn SearchableToolSet>> = vec![Arc::new(StubToolSet::tunnel(
            "stg-k8s",
            "staging",
            new_session,
        ))];
        toolsets.replace_tunnel_toolsets("staging", new_sets);

        // Both old "staging" entries gone; static "concourse" untouched;
        // new "stg-k8s" appended.
        assert_eq!(
            toolsets.toolset_names_for_test(),
            vec!["concourse", "stg-k8s"]
        );
    }

    /// Atomic swap only touches the target deployment — other
    /// deployments' tunnel toolsets are preserved.
    #[test]
    fn replace_tunnel_toolsets_spares_other_deployments() {
        let toolsets = ToolSets::empty_for_test();
        let staging_session = uuid::Uuid::new_v4();
        let prod_session = uuid::Uuid::new_v4();
        toolsets.register_searchable(StubToolSet::tunnel("stg-k8s", "staging", staging_session));
        toolsets.register_searchable(StubToolSet::tunnel("prd-k8s", "production", prod_session));

        toolsets.replace_tunnel_toolsets("staging", Vec::new());

        assert_eq!(toolsets.toolset_names_for_test(), vec!["prd-k8s"]);
    }

    /// Session-aware unregister: after an eviction-style replace, the
    /// evicted loop's `unregister_searchable_by_session(old_session)` is a
    /// no-op — it only matches entries whose scope carries `old_session`,
    /// of which there are none after the swap. The new session's entries
    /// survive.
    #[test]
    fn unregister_by_session_is_noop_for_evicted_session() {
        let toolsets = ToolSets::empty_for_test();
        let old_session = uuid::Uuid::new_v4();
        let new_session = uuid::Uuid::new_v4();

        toolsets.register_searchable(StubToolSet::tunnel("stg-k8s", "staging", old_session));
        toolsets.replace_tunnel_toolsets(
            "staging",
            vec![Arc::new(StubToolSet::tunnel(
                "stg-k8s",
                "staging",
                new_session,
            ))],
        );

        // Evicted loop's late cleanup must not remove the new session's entries.
        toolsets.unregister_searchable_by_session(old_session);
        assert_eq!(toolsets.toolset_names_for_test(), vec!["stg-k8s"]);

        // The real owner can still release itself.
        toolsets.unregister_searchable_by_session(new_session);
        assert!(toolsets.toolset_names_for_test().is_empty());
    }

    /// Clean disconnect path (no takeover): a session registers toolsets,
    /// then `unregister_searchable_by_session` with its own id cleans them up.
    #[test]
    fn unregister_by_session_clean_disconnect() {
        let toolsets = ToolSets::empty_for_test();
        let session = uuid::Uuid::new_v4();
        toolsets.register_searchable(StubToolSet::tunnel("stg-k8s", "staging", session));
        toolsets.register_searchable(StubToolSet::static_("concourse"));

        toolsets.unregister_searchable_by_session(session);
        // Static toolsets are never removed by session-based unregister.
        assert_eq!(toolsets.toolset_names_for_test(), vec!["concourse"]);
    }
}
