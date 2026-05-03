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
    Bash, CallCatalogTool, ComposeTool, ComposeTypes, Delete, DescribeCatalogTool, GlobTool, Grep,
    Ls, MoveFile, NotesTool, ProjectAgent, ProjectLog, ProjectSandbox, Read, SearchCatalog,
    SkillTool, SpacesTool, TextEditor, UseSkillTool, WhoAmI, WorkflowTool,
};
pub use traits::*;

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use rmcp::model::{CallToolResult, JsonObject};

use crate::audit::Audit;
use crate::auth::AuthSubject;

/// schemars 0.8 emits boolean `true` for `serde_json::Value` fields, which
/// strict JSON-Schema validators (notably Claude Code's MCP client) reject
/// inside `properties`. Use via `#[schemars(schema_with = "...")]` on a field
/// whose runtime type is `serde_json::Value` and whose contents are arbitrary
/// — emits the equivalent empty-object schema `{}`.
pub(crate) fn any_json_schema(_: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
    schemars::schema::Schema::Object(Default::default())
}

/// Companion to [`any_json_schema`] for `Vec<serde_json::Value>` fields —
/// emits `{ "type": "array", "items": {} }` so the items schema is an
/// object, not the boolean shorthand schemars defaults to.
pub(crate) fn array_of_any_schema(
    _: &mut schemars::gen::SchemaGenerator,
) -> schemars::schema::Schema {
    use schemars::schema::{ArrayValidation, InstanceType, Schema, SchemaObject, SingleOrVec};
    Schema::Object(SchemaObject {
        instance_type: Some(InstanceType::Array.into()),
        array: Some(Box::new(ArrayValidation {
            items: Some(SingleOrVec::Single(Box::new(Schema::Object(
                Default::default(),
            )))),
            ..Default::default()
        })),
        ..Default::default()
    })
}

pub struct ToolSets {
    sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
    top_level: Arc<RwLock<HashMap<String, Arc<dyn TopLevelTool>>>>,
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
        let top_level: Arc<RwLock<HashMap<String, Arc<dyn TopLevelTool>>>> =
            Arc::new(RwLock::new(HashMap::new()));

        let search = Arc::new(SearchCatalog::new(Arc::clone(&sets)));
        let describe = Arc::new(DescribeCatalogTool::new(Arc::clone(&sets)));
        let call = Arc::new(CallCatalogTool::new(Arc::clone(&sets)));
        let compose = Arc::new(ComposeTool::new(
            Arc::clone(&sets),
            Arc::clone(&top_level),
            config.compose.clone(),
        ));
        let compose_types = Arc::new(ComposeTypes::new(Arc::clone(&sets), Arc::clone(&top_level)));
        let whoami = Arc::new(WhoAmI::new());

        {
            let mut map = top_level.write().expect("top_level lock poisoned");
            map.insert(search.name().to_string(), search as Arc<dyn TopLevelTool>);
            map.insert(
                describe.name().to_string(),
                describe as Arc<dyn TopLevelTool>,
            );
            map.insert(call.name().to_string(), call as Arc<dyn TopLevelTool>);
            map.insert(compose.name().to_string(), compose as Arc<dyn TopLevelTool>);
            map.insert(
                compose_types.name().to_string(),
                compose_types as Arc<dyn TopLevelTool>,
            );
            map.insert(whoami.name().to_string(), whoami as Arc<dyn TopLevelTool>);
        }

        Ok(Self {
            sets,
            top_level,
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

    /// Optional — when `None` (e.g. in tests) audit is silently skipped.
    pub fn set_audit(&mut self, audit: Arc<Audit>) {
        self.audit = Some(audit);
    }

    /// Uses interior mutability so tools can be registered after the
    /// `ToolSets` is wrapped in an `Arc`.
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

    /// Human-readable summary used as the MCP server's `instructions` payload.
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

    /// Top-level tools visible to `subject`. Included iff
    /// [`TopLevelTool::is_visible`] returns `true`.
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

    /// Records an audit entry when an [`Audit`] has been wired via [`set_audit`].
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

    /// Batch dispatch for the agent loop. Looks up `name` once, then
    /// asks each input for its [`TopLevelTool::batch_key`]: inputs with
    /// the same `Some(key)` go through [`TopLevelTool::call_batch`] as
    /// a unit; `None` keys go through [`TopLevelTool::call`] one at a
    /// time. Either way the dispatcher emits exactly one audit row per
    /// logical input — tools never deal with audit themselves.
    pub async fn call_top_level_tool_batch(
        &self,
        subject: &AuthSubject,
        name: &str,
        inputs: Vec<Option<JsonObject>>,
    ) -> Vec<Result<CallToolResult, ToolSetsError>> {
        if inputs.is_empty() {
            return Vec::new();
        }

        let tool = {
            let map = self.top_level.read().expect("top_level lock poisoned");
            match map.get(name) {
                Some(t) => Arc::clone(t),
                None => {
                    return inputs
                        .into_iter()
                        .map(|_| Err(ToolSetsError::ToolNotFound(name.to_string())))
                        .collect();
                }
            }
        };

        let mut singleton_indices: Vec<usize> = Vec::new();
        let mut batched: HashMap<&'static str, Vec<usize>> = HashMap::new();
        for (i, args) in inputs.iter().enumerate() {
            match tool.batch_key(args.as_ref()) {
                Some(key) => batched.entry(key).or_default().push(i),
                None => singleton_indices.push(i),
            }
        }

        let mut singleton_futs = Vec::with_capacity(singleton_indices.len());
        for i in &singleton_indices {
            let tool = Arc::clone(&tool);
            let subject = subject.clone();
            let name = name.to_string();
            let args = inputs[*i].clone();
            let audit = self.audit.clone();
            let idx = *i;
            singleton_futs.push(async move {
                let result = run_single_with_audit(&*tool, &subject, &name, args, audit).await;
                vec![(idx, result)]
            });
        }

        let mut batch_futs = Vec::with_capacity(batched.len());
        for (_key, indices) in batched.into_iter() {
            let tool = Arc::clone(&tool);
            let subject = subject.clone();
            let name = name.to_string();
            let group_inputs: Vec<Option<JsonObject>> =
                indices.iter().map(|i| inputs[*i].clone()).collect();
            let audit = self.audit.clone();
            batch_futs.push(async move {
                run_batch_with_audit(&*tool, &subject, &name, indices, group_inputs, audit).await
            });
        }

        let (singleton_groups, batch_groups) = tokio::join!(
            futures::future::join_all(singleton_futs),
            futures::future::join_all(batch_futs),
        );

        let mut by_index: Vec<Option<Result<CallToolResult, ToolSetsError>>> =
            (0..inputs.len()).map(|_| None).collect();
        for group in singleton_groups {
            for (i, r) in group {
                by_index[i] = Some(r);
            }
        }
        for group in batch_groups {
            for (i, r) in group {
                by_index[i] = Some(r);
            }
        }

        by_index
            .into_iter()
            .map(|slot| slot.expect("every input must produce a result"))
            .collect()
    }
}

/// One audit row + one `tool.call(...)` invocation. Mirrors the wrapping
/// in [`ToolSets::call_top_level_tool`] so single-call semantics stay
/// identical whether the agent dispatcher batches or not.
async fn run_single_with_audit(
    tool: &dyn TopLevelTool,
    subject: &AuthSubject,
    name: &str,
    arguments: Option<JsonObject>,
    audit: Option<Arc<Audit>>,
) -> Result<CallToolResult, ToolSetsError> {
    use es_entity::context::{EventContext, WithEventContext};

    let seed = {
        let ctx = EventContext::current();
        ctx.data()
    };
    let metadata = serde_json::json!({
        "tool_name": name,
        "arguments": arguments
            .as_ref()
            .map(|a| serde_json::Value::Object(a.clone())),
    });

    async move {
        Audit::record_subject(subject);
        Audit::record_entrypoint(format!("mcp: {name}"));
        Audit::record_interaction_type(crate::audit::primitives::InteractionType::McpCall);
        Audit::record_action_if_unset(name);
        Audit::record_metadata(metadata);

        let start = std::time::Instant::now();
        let result = tool.call(subject, arguments).await;
        Audit::record_duration(start);
        match &result {
            Ok(r) => {
                Audit::record_tokens(estimate_tokens(r));
                Audit::record_success();
            }
            Err(e) => Audit::record_error(e.to_string()),
        }
        if let Some(audit) = audit.as_ref() {
            audit.record_from_context();
        }
        result
    }
    .with_event_context(seed)
    .await
}

/// One `tool.call_batch(...)` invocation + N audit rows (one per
/// logical input). Tools never see the audit context — the dispatcher
/// records subject/entrypoint/duration/tokens/error around each result.
async fn run_batch_with_audit(
    tool: &dyn TopLevelTool,
    subject: &AuthSubject,
    name: &str,
    indices: Vec<usize>,
    group_inputs: Vec<Option<JsonObject>>,
    audit: Option<Arc<Audit>>,
) -> Vec<(usize, Result<CallToolResult, ToolSetsError>)> {
    use es_entity::context::{EventContext, WithEventContext};

    let seed = {
        let ctx = EventContext::current();
        ctx.data()
    };

    let start = std::time::Instant::now();
    let results = tool.call_batch(subject, group_inputs.clone()).await;

    for (input, result) in group_inputs.iter().zip(&results) {
        let metadata = serde_json::json!({
            "tool_name": name,
            "arguments": input
                .as_ref()
                .map(|a| serde_json::Value::Object(a.clone())),
            "batched": true,
        });
        let result_summary = match result {
            Ok(r) => Ok(estimate_tokens(r)),
            Err(e) => Err(e.to_string()),
        };
        let audit = audit.clone();
        async {
            Audit::record_subject(subject);
            Audit::record_entrypoint(format!("mcp: {name}"));
            Audit::record_interaction_type(crate::audit::primitives::InteractionType::McpCall);
            Audit::record_action_if_unset(name);
            Audit::record_metadata(metadata);
            Audit::record_duration(start);
            match result_summary {
                Ok(tokens) => {
                    Audit::record_tokens(tokens);
                    Audit::record_success();
                }
                Err(msg) => Audit::record_error(msg),
            }
            if let Some(audit) = audit.as_ref() {
                audit.record_from_context();
            }
        }
        .with_event_context(seed.clone())
        .await;
    }

    indices.into_iter().zip(results).collect()
}

/// Estimate tokens from text content (~4 chars per token).
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
    pub fn empty_for_test() -> Self {
        Self {
            sets: Arc::new(RwLock::new(Vec::new())),
            top_level: Arc::new(RwLock::new(HashMap::new())),
            audit: None,
            init_errors: Vec::new(),
        }
    }

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

        assert_eq!(
            toolsets.toolset_names_for_test(),
            vec!["concourse", "stg-k8s"]
        );
    }

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

        toolsets.unregister_searchable_by_session(old_session);
        assert_eq!(toolsets.toolset_names_for_test(), vec!["stg-k8s"]);

        toolsets.unregister_searchable_by_session(new_session);
        assert!(toolsets.toolset_names_for_test().is_empty());
    }

    #[test]
    fn unregister_by_session_clean_disconnect() {
        let toolsets = ToolSets::empty_for_test();
        let session = uuid::Uuid::new_v4();
        toolsets.register_searchable(StubToolSet::tunnel("stg-k8s", "staging", session));
        toolsets.register_searchable(StubToolSet::static_("concourse"));

        toolsets.unregister_searchable_by_session(session);
        assert_eq!(toolsets.toolset_names_for_test(), vec!["concourse"]);
    }
}
