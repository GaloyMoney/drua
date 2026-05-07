mod config;
mod error;
mod filter;
mod inspect;
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
    SkillTool, SpacesTool, SubmitOutputTool, TextEditor, UseSkillTool, WhoAmI, WorkflowTool,
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

// OpenAI strict-mode invariants: every key in `properties` must appear
// in `required`, and every object must set `additionalProperties: false`.
// `submit_output` ships with `strict: true`, so user-supplied output
// schemas — which may reasonably mark fields as optional — must be lifted
// to that shape. Naturally-optional fields stay semantically optional by
// becoming nullable on the wire.
fn normalize_for_strict(mut value: serde_json::Value) -> serde_json::Value {
    normalize_for_strict_in_place(&mut value);
    value
}

fn normalize_for_strict_in_place(value: &mut serde_json::Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };

    if obj.get("type").and_then(|t| t.as_str()) != Some("object") {
        if let Some(items) = obj.get_mut("items") {
            normalize_for_strict_in_place(items);
        }
        return;
    }

    let originally_required: std::collections::HashSet<String> = obj
        .get("required")
        .and_then(|r| r.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    if let Some(props) = obj.get_mut("properties").and_then(|v| v.as_object_mut()) {
        let all_keys: Vec<String> = props.keys().cloned().collect();

        for key in &all_keys {
            let Some(prop) = props.get_mut(key) else {
                continue;
            };
            normalize_for_strict_in_place(prop);

            if !originally_required.contains(key) {
                if let Some(prop_obj) = prop.as_object_mut() {
                    if let Some(t) = prop_obj.get("type").cloned() {
                        let new_type = match t {
                            serde_json::Value::String(s) if s != "null" => {
                                serde_json::json!([s, "null"])
                            }
                            serde_json::Value::Array(mut arr) => {
                                if !arr.iter().any(|v| v.as_str() == Some("null")) {
                                    arr.push(serde_json::json!("null"));
                                }
                                serde_json::Value::Array(arr)
                            }
                            other => other,
                        };
                        prop_obj.insert("type".to_string(), new_type);
                    }
                }
            }
        }

        obj.insert("required".to_string(), serde_json::json!(all_keys));
    } else {
        obj.insert("required".to_string(), serde_json::json!([]));
    }

    obj.insert("additionalProperties".to_string(), serde_json::json!(false));
}

pub struct ToolSets {
    sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
    top_level: Arc<RwLock<HashMap<String, Arc<dyn TopLevelTool>>>>,
    /// `None` only in tests without a DB pool.
    audit: Option<Arc<Audit>>,
    init_errors: Vec<(String, String)>,
}

impl ToolSets {
    pub async fn init(
        config: ToolSetsConfig,
        audit: Option<Arc<Audit>>,
    ) -> Result<Self, ToolSetsError> {
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

        if config.zenduty.enabled && !config.zenduty.api_token.is_empty() {
            match zenduty_client::ZendutyClient::new(&config.zenduty.url, &config.zenduty.api_token)
            {
                Ok(client) => {
                    let default_team = if config.zenduty.default_team.is_empty() {
                        None
                    } else {
                        Some(config.zenduty.default_team.clone())
                    };
                    sets.push(Arc::new(ZendutyToolSet::new(client, default_team)));
                    tracing::info!(
                        url = %config.zenduty.url,
                        "Zenduty toolset initialized"
                    );
                }
                Err(e) => {
                    init_errors.push(("zenduty".to_string(), e.to_string()));
                }
            }
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
            audit.clone(),
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
            audit,
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
    /// Visible top-level tool *trait objects* for a subject. Used by
    /// the MCP gateway and other consumers that need the raw tool
    /// definitions; the agent layer should use [`Self::top_level_tool_defs`]
    /// which also applies per-agent schema substitutions (e.g.
    /// `submit_output`).
    pub fn top_level_tool_arcs(
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

    /// Visible top-level tools converted to wire `ToolDefinition`s
    /// for the session layer. When `output_schema` is `Some` the
    /// `submit_output` entry's `input_schema` is replaced with the
    /// agent's per-step schema (overriding the global tool's
    /// permissive placeholder); strict mode is also enabled.
    pub fn top_level_tool_defs(
        &self,
        subject: &AuthSubject,
        output_schema: Option<&crate::workflow::OutputSchema>,
    ) -> Vec<crate::agent::session::message::ToolDefinition> {
        use crate::agent::session::message::{ToolDefinition, SUBMIT_OUTPUT_TOOL_NAME};

        let mut defs: Vec<ToolDefinition> = self
            .top_level_tool_arcs(subject)
            .map(|t| ToolDefinition::from(llm::prompt::Tool::from(t.as_ref())))
            .collect();

        if let Some(schema) = output_schema {
            let real_input_schema = normalize_for_strict(
                serde_json::to_value(schema.root_schema())
                    .expect("OutputSchema serialises to JSON"),
            );
            let real_def = ToolDefinition {
                name: SUBMIT_OUTPUT_TOOL_NAME.to_string(),
                description: Some(
                    "Call this exactly once to record this step's structured \
                     result and finish the step."
                        .to_string(),
                ),
                input_schema: real_input_schema,
                strict: true,
            };
            match defs.iter().position(|t| t.name == SUBMIT_OUTPUT_TOOL_NAME) {
                Some(idx) => defs[idx] = real_def,
                None => defs.push(real_def),
            }
        }

        defs
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

    #[test]
    fn normalize_for_strict_promotes_all_properties_to_required() {
        let input = serde_json::json!({
            "type": "object",
            "required": ["a"],
            "properties": {
                "a": { "type": "string" },
                "b": { "type": "string" }
            }
        });
        let out = normalize_for_strict(input);
        let mut required: Vec<String> = out["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        required.sort();
        assert_eq!(required, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn normalize_for_strict_makes_optional_fields_nullable() {
        let input = serde_json::json!({
            "type": "object",
            "required": [],
            "properties": {
                "a": { "type": "string" }
            }
        });
        let out = normalize_for_strict(input);
        assert_eq!(
            out["properties"]["a"]["type"],
            serde_json::json!(["string", "null"])
        );
    }

    #[test]
    fn normalize_for_strict_keeps_originally_required_fields_non_nullable() {
        let input = serde_json::json!({
            "type": "object",
            "required": ["a"],
            "properties": {
                "a": { "type": "string" }
            }
        });
        let out = normalize_for_strict(input);
        assert_eq!(out["properties"]["a"]["type"], serde_json::json!("string"));
    }

    #[test]
    fn normalize_for_strict_sets_additional_properties_false() {
        let input = serde_json::json!({
            "type": "object",
            "properties": {
                "a": { "type": "string" }
            }
        });
        let out = normalize_for_strict(input);
        assert_eq!(out["additionalProperties"], serde_json::json!(false));
    }

    #[test]
    fn normalize_for_strict_idempotent_on_already_strict_schema() {
        let strict = serde_json::json!({
            "type": "object",
            "required": ["a", "b"],
            "additionalProperties": false,
            "properties": {
                "a": { "type": "string" },
                "b": { "type": ["string", "null"] }
            }
        });
        let out = normalize_for_strict(strict.clone());
        assert_eq!(out, strict);
    }

    #[test]
    fn normalize_for_strict_recurses_into_nested_objects() {
        let input = serde_json::json!({
            "type": "object",
            "required": ["nested"],
            "properties": {
                "nested": {
                    "type": "object",
                    "required": [],
                    "properties": {
                        "inner": { "type": "string" }
                    }
                }
            }
        });
        let out = normalize_for_strict(input);
        assert_eq!(
            out["properties"]["nested"]["additionalProperties"],
            serde_json::json!(false)
        );
        assert_eq!(
            out["properties"]["nested"]["required"],
            serde_json::json!(["inner"])
        );
        assert_eq!(
            out["properties"]["nested"]["properties"]["inner"]["type"],
            serde_json::json!(["string", "null"])
        );
    }

    #[test]
    fn normalize_for_strict_default_output_schema() {
        let schema = crate::workflow::default_output_schema();
        let value = serde_json::to_value(schema.root_schema()).expect("serialises");
        let out = normalize_for_strict(value);
        assert_eq!(out["additionalProperties"], serde_json::json!(false));
        let mut required: Vec<String> = out["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        required.sort();
        assert_eq!(
            required,
            vec![
                "output".to_string(),
                "reason".to_string(),
                "success".to_string()
            ]
        );
        assert_eq!(
            out["properties"]["reason"]["type"],
            serde_json::json!(["string", "null"])
        );
        assert_eq!(
            out["properties"]["output"]["type"],
            serde_json::json!("string")
        );
        assert_eq!(
            out["properties"]["success"]["type"],
            serde_json::json!("boolean")
        );
    }

    #[test]
    fn top_level_tool_defs_emits_strict_compatible_submit_output() {
        use crate::agent::session::message::SUBMIT_OUTPUT_TOOL_NAME;

        let toolsets = ToolSets::empty_for_test();
        let schema = crate::workflow::default_output_schema();
        let defs = toolsets.top_level_tool_defs(&AuthSubject::Anonymous, Some(&schema));
        let submit = defs
            .into_iter()
            .find(|d| d.name == SUBMIT_OUTPUT_TOOL_NAME)
            .expect("submit_output emitted when output_schema is Some");

        assert!(submit.strict, "submit_output must be strict");
        assert_eq!(
            submit.input_schema["additionalProperties"],
            serde_json::json!(false)
        );

        let required: std::collections::HashSet<String> = submit.input_schema["required"]
            .as_array()
            .expect("required is an array")
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        let props: std::collections::HashSet<String> = submit.input_schema["properties"]
            .as_object()
            .expect("properties is an object")
            .keys()
            .cloned()
            .collect();
        assert_eq!(
            required, props,
            "every key in properties must appear in required for strict mode"
        );
    }
}
