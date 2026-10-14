//! Execute JavaScript that chains multiple MCP tool calls in a single round trip.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::Duration;

use drua_library::SpaceError;
use drua_tool_caching::{
    extract_text, fetch_text_for_raw, tool_result_value, ToolCaching, ToolOutputShape,
};
use es_entity::context::{EventContext, WithEventContext};
use rmcp::model::{CallToolResult, JsonObject};
use serde::Deserialize;

use crate::audit::{Audit, InteractionType};
use crate::auth::AuthSubject;
use crate::project::ProjectError;
use crate::space_fs::SpaceFs;

use super::super::config::ComposeConfig;
use super::super::error::ToolSetsError;
use super::super::traits::{SearchableToolSet, TopLevelTool};
use super::{parse_params, schema_for};

#[derive(Deserialize, schemars::JsonSchema)]
struct ComposeParams {
    script: String,
}

#[derive(Clone)]
pub struct ComposeTool {
    sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
    top_level: Arc<RwLock<HashMap<String, Arc<dyn TopLevelTool>>>>,
    audit: Option<Arc<Audit>>,
    tool_caching: Option<Arc<ToolCaching>>,
    config: ComposeConfig,
    // Absent only in standalone inline-compose test harnesses.
    space_fs: Option<Arc<SpaceFs>>,
}

impl ComposeTool {
    /// Instance scoped to a `script_step` dispatch: `max_tool_calls` and
    /// `timeout_ms` are overridden up to (never beyond) the
    /// `script_step` config ceiling; `None` uses the ceiling itself.
    /// Every other config field (memory/stack/console/return limits,
    /// loader budgets) is unchanged — those aren't per-step overridable.
    pub(crate) fn for_script(
        &self,
        max_tool_calls: Option<usize>,
        timeout_ms: Option<u64>,
    ) -> Self {
        let ceiling = self.config.script_step;
        let mut tool = self.clone();
        tool.config.max_tool_calls =
            max_tool_calls.map_or(ceiling.max_tool_calls, |v| v.min(ceiling.max_tool_calls));
        tool.config.timeout_ms =
            timeout_ms.map_or(ceiling.timeout_ms, |v| v.min(ceiling.timeout_ms));
        tool
    }

    pub fn new(
        sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
        top_level: Arc<RwLock<HashMap<String, Arc<dyn TopLevelTool>>>>,
        audit: Option<Arc<Audit>>,
        tool_caching: Option<Arc<ToolCaching>>,
        config: ComposeConfig,
        space_fs: Arc<SpaceFs>,
    ) -> Self {
        Self {
            sets,
            top_level,
            audit,
            tool_caching,
            config,
            space_fs: Some(space_fs),
        }
    }

    pub(crate) fn without_space_fs_for_test(
        sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
        top_level: Arc<RwLock<HashMap<String, Arc<dyn TopLevelTool>>>>,
        audit: Option<Arc<Audit>>,
        tool_caching: Option<Arc<ToolCaching>>,
        config: ComposeConfig,
    ) -> Self {
        Self {
            sets,
            top_level,
            audit,
            tool_caching,
            config,
            space_fs: None,
        }
    }
}

// Caller-bound adapter to ordinary authorized space reads.
struct SpaceScriptProvider {
    fs: Arc<SpaceFs>,
    subject: AuthSubject,
}

#[async_trait::async_trait]
impl js_engine::ScriptSourceProvider for SpaceScriptProvider {
    async fn read(&self, path: &str) -> Result<Vec<u8>, String> {
        self.fs
            .read_file_bytes(&self.subject, path)
            .await
            .map_err(|error| match error {
                ProjectError::Authorization(_)
                | ProjectError::Space(SpaceError::NotMounted { .. }) => {
                    "access_denied: space is not accessible".to_owned()
                }
                ProjectError::Space(
                    SpaceError::PathNotFound { .. } | SpaceError::NotFound { .. },
                ) => {
                    format!("missing_file: {error}")
                }
                _ => format!("read_error: {error}"),
            })?
            .ok_or_else(|| "invalid_path: expected a space path".to_owned())
    }
}

static COMPOSE_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<ComposeParams>);

#[derive(serde::Serialize, schemars::JsonSchema)]
struct ComposeOutput {
    /// Recoverable sub-tool calls; excludes passthrough, errored, and bypass-marked calls.
    /// Note: serde_json without `preserve_order` emits object fields alphabetically,
    /// so the agent sees `fetch_hint` before `result` before `sub_invocations` regardless
    /// of source order. The drill-down nudge reaches the agent through `fetch_hint`.
    sub_invocations: Vec<SubInvocation>,
    fetch_hint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    normal_fetch_limit_bytes: Option<u64>,
    tool_calls: usize,
    execution_time_ms: u64,
    console: Vec<String>,
    /// The JS script's return value, verbatim. If oversize, the outer
    /// `cache()` call walks the full ComposeOutput tree and elides this
    /// field in place; the agent recovers via `tool_output_fetch` with
    /// `path: "$.result"` (compose opts out of the `DruaToolResult`
    /// wrap, so there's no outer `result` key — the persisted root IS
    /// `ComposeOutput`) or `query: {mode: "summary"}` for the full
    /// envelope, which bypasses the normal fetch cap. Per-subcall
    /// `summary_envelope_bytes` exposes the size before fetching.
    #[schemars(schema_with = "crate::toolset::any_json_schema")]
    result: serde_json::Value,
}

/// `ComposeOutput` is generated with `additionalProperties: false` by
/// `schema_for`, but `cache_envelope` merges `_recovery` into the
/// ComposeOutput root when the walker elides. Without declaring it as an
/// optional property here, strict MCP clients reject every elided
/// compose response. Mirrors the recovery property shape in `wrap.rs`.
static COMPOSE_OUTPUT_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    let mut schema = schema_for::<ComposeOutput>();
    crate::toolset::wrap::merge_envelope_recovery_props(&mut schema);
    schema
});

#[async_trait::async_trait]
impl TopLevelTool for ComposeTool {
    fn name(&self) -> &str {
        "compose"
    }

    fn description(&self) -> &str {
        "Execute JavaScript that composes multiple tool calls in a single round trip. \
         The script has access to a `tools` namespace with nested server namespaces \
         (e.g. `tools.honeycomb.list_environments({...})`). Flat prefixed names also work \
         (e.g. `tools.honeycomb_list_environments({...})`). \
         Use `return` for the final value. Top-level `await` and `Promise.all()` are supported. \
         **Call `compose_types` first** to fetch exact tool signatures and parameter names \
         — guessing leads to runtime errors that waste a round trip. \
         **Reduce before you return.** The script reads full upstream payloads for free, \
         but the value you `return` is elided against the same ~8 KB budget as any tool \
         result — return the fields and rows you actually need, not whole responses, or \
         the agent spends `tool_output_fetch` round trips reading back what the script \
         already had in hand. \
         Scope is plain JavaScript plus `tools`, `console`, and `setTimeout` — \
         no Node.js builtins (`require`, `module`, `process`, `fs` are unavailable).\n\n\
         Example:\n```js\nconst envs = await tools.honeycomb.list_environments({});\n\
         const issues = await tools.github.list_issues({ repo: 'org/repo', state: 'open' });\n\
         const stale = issues.filter(i => Date.now() - Date.parse(i.updated_at) > 7*86400*1000);\n\
         return { envs, stale_issues: stale.map(i => i.number) };\n```\n\n\
         Load reusable space helpers with `const helper = await loadScript(\"space:tools/helper.js\");` \
         and call their exported functions with arguments. Files are async function bodies that \
         explicitly return exports. Dependencies use the same loader and caller permissions. \
         Initialization is cached for this invocation; keep it pure and put mutations in exported functions."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &COMPOSE_SCHEMA
    }

    fn inner_output_schema(&self) -> Option<&serde_json::Value> {
        Some(&COMPOSE_OUTPUT_SCHEMA)
    }

    fn composable(&self) -> bool {
        false
    }

    fn default_tool_caching(&self) -> bool {
        false
    }

    #[tracing::instrument(name = "toolset.compose.call", skip_all)]
    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: ComposeParams = parse_params(arguments)?;

        let timeout = Duration::from_millis(self.config.timeout_ms);

        Audit::record_action("compose".to_string());

        let recorded_args = serde_json::json!({
            "script": params.script,
        });

        let dts = {
            let sets = self.sets.read().expect("toolset lock poisoned");
            let top = self.top_level.read().expect("top_level lock poisoned");
            generate_dts(subject, &sets, &top)
        };

        let script = if dts.is_empty() {
            params.script
        } else {
            format!("/*\n{dts}*/\n{}", params.script)
        };

        let sub_invocations: Arc<Mutex<Vec<SubInvocation>>> = Arc::new(Mutex::new(Vec::new()));
        let dispatcher = Arc::new(CatalogDispatcher {
            sets: Arc::clone(&self.sets),
            top_level: Arc::clone(&self.top_level),
            subject: subject.clone(),
            audit: self.audit.clone(),
            tool_caching: self.tool_caching.clone(),
            sub_invocations: Arc::clone(&sub_invocations),
            memo: ScriptMemo::new(self.config.max_memo_bytes),
        });

        let engine = js_engine::JsEngine::new()
            .with_script_limits(js_engine::ScriptLimits {
                max_file_bytes: self.config.max_script_file_bytes,
                max_total_bytes: self.config.max_script_total_bytes,
                max_loads: self.config.max_script_loads,
                max_dependency_depth: self.config.max_script_dependency_depth,
            })
            .with_max_tool_calls(self.config.max_tool_calls)
            .with_max_tool_result_bytes(self.config.max_tool_result_bytes)
            .with_max_return_bytes(self.config.max_return_bytes)
            .with_max_console_bytes(self.config.max_console_bytes)
            .with_memory_limit(self.config.memory_limit_bytes)
            .with_stack_limit(self.config.stack_limit_bytes);
        let script_audit: js_engine::SharedScriptAudit = Arc::default();
        let result = engine
            .execute_with_sources(
                &script,
                dispatcher,
                timeout,
                self.space_fs.as_ref().map(|fs| {
                    Arc::new(SpaceScriptProvider {
                        fs: Arc::clone(fs),
                        subject: subject.clone(),
                    }) as Arc<dyn js_engine::ScriptSourceProvider>
                }),
                script_audit.clone(),
            )
            .await;
        Audit::merge_metadata(serde_json::json!({ "script_execution": {
            "loads": &*script_audit.lock().unwrap(),
            "error": result.as_ref().err().map(ToString::to_string),
        }}));
        let result = result.map_err(|e| ToolSetsError::Compose(e.to_string()))?;

        let collected_sub_invocations = sub_invocations
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();

        // Compose opts out of the `DruaToolResult` wrap, so the
        // persisted root IS `ComposeOutput`: agents recover via
        // `path: "$.result"` (not `$.result.result`) or
        // `query: {mode: "summary"}` for the full curated envelope,
        // which bypasses the normal fetch cap. The sub-invocation
        // metadata exposes summary size before the agent fetches it.
        let out = ComposeOutput {
            sub_invocations: collected_sub_invocations,
            fetch_hint: COMPOSE_FETCH_HINT.to_string(),
            normal_fetch_limit_bytes: self
                .tool_caching
                .as_ref()
                .map(|tc| tc.max_fetch_response_bytes()),
            tool_calls: result.tool_calls_made,
            execution_time_ms: result.execution_time.as_millis() as u64,
            console: result.console_output.clone(),
            result: result.value.clone(),
        };

        Audit::merge_metadata(serde_json::json!({"compose": {
            "tool_calls": out.tool_calls,
            "execution_time_ms": out.execution_time_ms,
            "console": out.console,
        }}));
        tracing::info!(
            tool_calls = out.tool_calls,
            execution_time_ms = out.execution_time_ms,
            "compose completed"
        );
        let structured = serde_json::to_value(&out).expect("ComposeOutput serialization");
        let mut ctr = CallToolResult::success(Vec::new());
        ctr.structured_content = Some(structured);

        let ctr = match self.tool_caching.as_ref() {
            Some(tc) => {
                // `cache_envelope()` — compose advertises `ComposeOutput`
                // (a flat schema with no outer `{ result: ... }` wrapper)
                // as its outputSchema, so it must NOT go through `cache()`'s
                // DruaToolResult wrap: strict MCP clients validate
                // `structuredContent` against the advertised schema and
                // reject the wrapped shape (which nests ComposeOutput under
                // a `result` key, dropping its declared top-level fields).
                // `cache_envelope` walks/persists identically but emits `T`
                // verbatim, merging `_recovery` into the ComposeOutput
                // root when the walker elides `result`.
                tc.cache_envelope(subject, "compose", &recorded_args, ctr, self.output_shape())
                    .await?
                    .result
            }
            None => ctr,
        };
        Ok(ctr)
    }
}

const COMPOSE_FETCH_HINT: &str =
    "Use any `sub_invocations[].invocation_id` with `tool_output_fetch` to fetch a \
     sub-call's persisted output; `query: {mode: \"summary\"}` returns the curated \
     `<summary>+<recovery>` envelope you'd have seen calling that tool directly. \
     Compose includes `normal_fetch_limit_bytes`, and each sub_invocation includes \
     `summary_envelope_bytes`, so you can size summary fetches before calling them.";

/// Recovery metadata for one sub-tool dispatch inside a compose script.
/// Array order is completion order (0-based).
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct SubInvocation {
    pub tool_name: String,
    /// `tool(key=value, ...)` truncated to 80 chars.
    pub args_digest: String,
    /// `tool_invocations.id` — pass to `tool_output_fetch`.
    pub invocation_id: uuid::Uuid,
    /// Root path of the summary view for this persisted sub-call.
    pub root_path: String,
    /// Summary kind discriminator (`lines`, `range`, `json_array_slice`,
    /// `summarized`) — hint for which `tool_output_fetch` query mode
    /// would naturally slice this sub-call's output.
    pub kind: String,
    pub raw_size_bytes: u64,
    /// Size of `tool_output_fetch(query:{mode:"summary"})` for this
    /// sub-call's curated `<summary>+<recovery>` envelope.
    pub summary_envelope_bytes: u64,
}

struct CatalogDispatcher {
    sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
    top_level: Arc<RwLock<HashMap<String, Arc<dyn TopLevelTool>>>>,
    subject: AuthSubject,
    audit: Option<Arc<Audit>>,
    tool_caching: Option<Arc<ToolCaching>>,
    sub_invocations: Arc<Mutex<Vec<SubInvocation>>>,
    memo: ScriptMemo,
}

#[derive(Default)]
struct MemoState {
    entries: HashMap<(String, String), serde_json::Value>,
    bytes: usize,
    generation: u64,
}

// Lives and dies with one `CatalogDispatcher` (one compose invocation, one
// caller identity); never shared across invocations, agents or users.
struct ScriptMemo {
    state: Mutex<MemoState>,
    max_bytes: usize,
}

impl ScriptMemo {
    fn new(max_bytes: usize) -> Self {
        Self {
            state: Mutex::new(MemoState::default()),
            max_bytes,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MemoState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn get(&self, key: &(String, String)) -> Option<serde_json::Value> {
        self.lock().entries.get(key).cloned()
    }

    fn generation(&self) -> u64 {
        self.lock().generation
    }

    fn invalidate(&self) {
        let mut state = self.lock();
        state.entries.clear();
        state.bytes = 0;
        state.generation += 1;
    }

    /// Skipped when an invalidation landed since `generation` was read, so
    /// a read that raced a write can't cache pre-write state.
    fn insert(&self, generation: u64, key: (String, String), value: serde_json::Value) {
        let size = serde_json::to_vec(&value).map_or(usize::MAX, |b| b.len());
        let mut state = self.lock();
        if state.generation != generation || state.bytes.saturating_add(size) > self.max_bytes {
            return;
        }
        state.bytes += size;
        state.entries.insert(key, value);
    }
}

fn canonical_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), canonical_json(v)))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(canonical_json).collect())
        }
        other => other.clone(),
    }
}

fn memo_key(name: &str, args: &serde_json::Value) -> (String, String) {
    (name.to_string(), canonical_json(args).to_string())
}

fn record_memo_hit_context(action: String, name: &str, args: &serde_json::Value) {
    Audit::record_action(action);
    Audit::record_entrypoint(format!("compose > mcp: {name}"));
    Audit::record_interaction_type(InteractionType::McpCall);
    Audit::record_metadata(serde_json::json!({
        "tool_name": name,
        "arguments": args,
        "memo_hit": true,
    }));
    Audit::record_duration(std::time::Instant::now());
    Audit::record_tokens(0);
    Audit::record_success();
}

#[async_trait::async_trait]
impl js_engine::ToolDispatcher for CatalogDispatcher {
    async fn call_tool(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let Some(action) = self.memo_action(name) else {
            self.memo.invalidate();
            let result = self.dispatch(name, args).await;
            self.memo.invalidate();
            return result;
        };

        let key = memo_key(name, &args);
        if let Some(hit) = self.memo.get(&key) {
            let audit = self.audit.clone();
            let name = name.to_string();
            let parent_seed = EventContext::current().data();
            async move {
                record_memo_hit_context(action, &name, &args);
                if let Some(audit) = audit.as_ref() {
                    audit.record_from_context();
                }
            }
            .with_event_context(parent_seed)
            .await;
            return Ok(hit);
        }

        let generation = self.memo.generation();
        let result = self.dispatch(name, args).await;
        if let Ok(value) = &result {
            self.memo.insert(generation, key, value.clone());
        }
        result
    }
}

impl CatalogDispatcher {
    /// `Some(audit action)` only when the name resolves, for this caller,
    /// to a tool that declares itself memoizable — the same visibility and
    /// composability gates `dispatch` applies, so a memo hit can never
    /// serve a tool the caller couldn't have dispatched.
    fn memo_action(&self, name: &str) -> Option<String> {
        if let Ok((set, tool_name)) = self.find_set(name) {
            return set
                .script_memoizable(&tool_name)
                .then(|| format!("compose > catalog: {name}"));
        }
        let map = self.top_level.read().expect("top_level lock poisoned");
        match map.get(name) {
            Some(t) if t.is_visible(&self.subject) && t.composable() && t.script_memoizable() => {
                Some(format!("compose > top_level: {name}"))
            }
            _ => None,
        }
    }

    async fn dispatch(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        if let Ok((set, tool_name)) = self.find_set(name) {
            let action = format!("compose > catalog: {name}");
            let audit = self.audit.clone();
            let subject = self.subject.clone();
            let name_owned = name.to_string();
            let args_for_meta = args.clone();
            let parent_seed = EventContext::current().data();
            let started_at = chrono::Utc::now();
            let dispatcher = self.clone_for_persistence();
            let output_shape = set.output_shape(&tool_name);

            return async move {
                Audit::record_action(action);
                // Own entrypoint, or these rows are indistinguishable from
                // the outer `mcp: compose` result and `tokens_returned`
                // (the raw sub-tool payload, which never reaches the model)
                // pollutes any per-entrypoint context accounting.
                Audit::record_entrypoint(format!("compose > mcp: {name_owned}"));
                Audit::record_interaction_type(InteractionType::McpCall);
                Audit::record_metadata(serde_json::json!({
                    "tool_name": name_owned,
                    "arguments": args_for_meta,
                }));

                let start = std::time::Instant::now();
                let dispatch_result = run_searchable_call(set, &subject, &tool_name, args.clone())
                    .await
                    .map_err(|e| with_hint(&name_owned, e));
                let duration_ms = start.elapsed().as_millis() as u64;
                Audit::record_duration(start);

                let result = match dispatch_result {
                    Ok((raw, value)) => {
                        Audit::record_tokens(super::super::estimate_tokens(&raw));
                        dispatcher
                            .maybe_persist_sub_invocation(
                                &name_owned,
                                &args,
                                &raw,
                                duration_ms,
                                started_at,
                                output_shape,
                            )
                            .await;
                        Audit::record_success();
                        Ok(value)
                    }
                    Err(msg) => {
                        Audit::record_error(msg.clone());
                        Err(msg)
                    }
                };
                if let Some(audit) = audit.as_ref() {
                    audit.record_from_context();
                }

                result
            }
            .with_event_context(parent_seed)
            .await;
        }

        self.call_top_level(name, args).await
    }

    fn find_set(
        &self,
        prefixed_name: &str,
    ) -> Result<(Arc<dyn SearchableToolSet>, String), ToolSetsError> {
        let sets = self.sets.read().expect("toolset lock poisoned");
        super::super::dispatch::find_searchable(sets.iter(), &self.subject, prefixed_name)
            .ok_or_else(|| ToolSetsError::ToolNotFound(prefixed_name.to_string()))
    }

    async fn call_top_level(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let tool = {
            let map = self.top_level.read().expect("top_level lock poisoned");
            match map.get(name) {
                Some(t) if t.is_visible(&self.subject) && t.composable() => Arc::clone(t),
                // Registered but non-composable (compose, call_tool): say so
                // instead of "not found" + a hint that suggests the failing
                // call itself.
                Some(t) if t.is_visible(&self.subject) => {
                    return Err(format!(
                        "Tool not callable inside compose scripts: {name} is a \
                         top-level MCP tool — call it directly, outside compose."
                    ));
                }
                _ => return Err(with_hint(name, format!("Tool not found: {name}"))),
            }
        };

        let action = format!("compose > top_level: {name}");
        let audit = self.audit.clone();
        let subject = self.subject.clone();
        let name_owned = name.to_string();
        let args_for_meta = args.clone();
        let parent_seed = EventContext::current().data();
        let started_at = chrono::Utc::now();
        let dispatcher = self.clone_for_persistence();
        let should_persist = tool.default_tool_caching();
        let output_shape = tool.output_shape();

        async move {
            Audit::record_action(action);
            Audit::record_entrypoint(format!("compose > mcp: {name_owned}"));
            Audit::record_interaction_type(InteractionType::McpCall);
            Audit::record_metadata(serde_json::json!({
                "tool_name": name_owned,
                "arguments": args_for_meta,
            }));

            let start = std::time::Instant::now();
            let dispatch_result = run_top_level_call(tool, &subject, args.clone())
                .await
                .map_err(|e| with_hint(&name_owned, e));
            let duration_ms = start.elapsed().as_millis() as u64;
            Audit::record_duration(start);

            let result = match dispatch_result {
                Ok((raw, value)) => {
                    Audit::record_tokens(super::super::estimate_tokens(&raw));
                    if should_persist {
                        dispatcher
                            .maybe_persist_sub_invocation(
                                &name_owned,
                                &args,
                                &raw,
                                duration_ms,
                                started_at,
                                output_shape,
                            )
                            .await;
                    }
                    Audit::record_success();
                    Ok(value)
                }
                Err(msg) => {
                    Audit::record_error(msg.clone());
                    Err(msg)
                }
            };
            if let Some(audit) = audit.as_ref() {
                audit.record_from_context();
            }

            result
        }
        .with_event_context(parent_seed)
        .await
    }

    fn clone_for_persistence(&self) -> CatalogDispatcherShared {
        CatalogDispatcherShared {
            tool_caching: self.tool_caching.clone(),
            subject: self.subject.clone(),
            sub_invocations: Arc::clone(&self.sub_invocations),
        }
    }
}

#[derive(Clone)]
struct CatalogDispatcherShared {
    tool_caching: Option<Arc<ToolCaching>>,
    subject: AuthSubject,
    sub_invocations: Arc<Mutex<Vec<SubInvocation>>>,
}

impl CatalogDispatcherShared {
    async fn maybe_persist_sub_invocation(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        raw: &CallToolResult,
        _duration_ms: u64,
        _started_at: chrono::DateTime<chrono::Utc>,
        shape: ToolOutputShape,
    ) {
        let Some(tc) = self.tool_caching.as_ref() else {
            return;
        };
        let raw_size = serde_json::to_vec(&tool_result_value(raw))
            .map(|bytes| bytes.len() as u64)
            .unwrap_or_else(|_| extract_text(raw).len() as u64);
        let Ok(resp) = tc
            .persist_for_compose(&self.subject, tool_name, args, raw.clone(), shape)
            .await
        else {
            return;
        };
        // Only emit a SubInvocation entry when persistence actually
        // happened — passthrough results have no recoverable id and
        // showing them would just clutter the metadata block.
        let Some(invocation_id) = resp.invocation_id else {
            return;
        };
        let Some(summary_fetch_info) = resp.summary_fetch_info else {
            return;
        };
        let kind = sub_invocation_kind(&resp.elided_paths);
        if let Ok(mut guard) = self.sub_invocations.lock() {
            guard.push(SubInvocation {
                tool_name: tool_name.to_string(),
                args_digest: format_args_digest(tool_name, args),
                invocation_id: uuid::Uuid::from(invocation_id),
                root_path: resp.root_path.unwrap_or_else(|| "$".to_string()),
                kind,
                raw_size_bytes: raw_size,
                summary_envelope_bytes: summary_fetch_info.summary_envelope_bytes,
            });
        }
    }
}

/// Pick a discriminator from the elided_paths' recover modes. Used in
/// the agent-facing metadata to hint at what kind of summarisation
/// happened.
fn sub_invocation_kind(elided_paths: &[drua_tool_caching::ElidedPath]) -> String {
    let mut modes: Vec<&str> = elided_paths
        .iter()
        .filter_map(|p| {
            p.recover
                .get("args_template")
                .and_then(|a| a.get("query"))
                .and_then(|q| q.get("mode"))
                .and_then(|m| m.as_str())
        })
        .collect();
    modes.sort_unstable();
    modes.dedup();
    if modes.is_empty() {
        "summarized".to_string()
    } else {
        modes.join("+")
    }
}

fn format_args_digest(tool_name: &str, args: &serde_json::Value) -> String {
    let pretty = serde_json::to_string(args).unwrap_or_default();
    let summary = format!("{tool_name}({pretty})");
    if summary.len() <= 80 {
        summary
    } else {
        format!("{}…", &summary[..79])
    }
}

async fn run_searchable_call(
    set: Arc<dyn SearchableToolSet>,
    subject: &AuthSubject,
    tool_name: &str,
    args: serde_json::Value,
) -> Result<(CallToolResult, serde_json::Value), String> {
    let inner_args = match args {
        serde_json::Value::Object(obj) => Some(obj),
        serde_json::Value::Null => None,
        _ => return Err(format!("Expected object arguments, got: {args}")),
    };

    // Defense-in-depth: JS values are usually well-typed, but if a script
    // passes stringified JSON we parse the same way the regular path does.
    let inner_args = inner_args.map(|mut a| {
        if let Some(entry) = set.tools().iter().find(|t| t.name == tool_name) {
            let schema = serde_json::Value::Object(entry.description.input_schema.as_ref().clone());
            super::super::auto_parse_args::coerce_args_to_schema(&mut a, &schema);
        }
        a
    });

    let result = set
        .call(subject, tool_name, inner_args)
        .await
        .map_err(|e| e.to_string())?;
    if result.is_error == Some(true) {
        return Err(format_tool_error(tool_name, &result));
    }

    let value = result_to_value(&result);
    Ok((result, value))
}

async fn run_top_level_call(
    tool: Arc<dyn TopLevelTool>,
    subject: &AuthSubject,
    args: serde_json::Value,
) -> Result<(CallToolResult, serde_json::Value), String> {
    let raw_args = args.clone();
    let inner_args = match args {
        serde_json::Value::Object(obj) => Some(obj),
        serde_json::Value::Null => None,
        _ => return Err(format!("Expected object arguments, got: {args}")),
    };

    // Same defense-in-depth as searchable runner.
    let inner_args = inner_args.map(|mut a| {
        super::super::auto_parse_args::coerce_args_to_schema(&mut a, tool.input_schema());
        a
    });

    let result = tool
        .call_from_script(subject, inner_args)
        .await
        .map_err(|e| e.to_string())?;
    if result.is_error == Some(true) {
        return Err(format_tool_error(tool.name(), &result));
    }

    let value = if tool.name() == "tool_output_fetch" {
        tool_output_fetch_compose_value(&raw_args, &result)
    } else {
        result_to_value(&result)
    };
    Ok((result, value))
}

/// JS-engine view of a sub-call's result. Returns the upstream's actual
/// structured shape when present, otherwise the same canonical content value
/// used by tool caching. No `{value|items, _shape}` envelope ever leaks into JS.
fn result_to_value(result: &CallToolResult) -> serde_json::Value {
    tool_result_value(result)
}

/// `tool_output_fetch` must expose object-shaped `structuredContent` to MCP
/// clients, so scalar and array roots are wrapped as `{result: ...}` at the
/// tool boundary. Compose scripts should still receive root scalar/array
/// recoveries directly, matching other tool calls' upstream-shaped `T`.
fn tool_output_fetch_compose_value(
    args: &serde_json::Value,
    result: &CallToolResult,
) -> serde_json::Value {
    let value = result_to_value(result);

    let fetch_path = args
        .get("path")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("$");
    let array_root_path = fetch_path.starts_with("$[");
    if fetch_path != "$" && !array_root_path {
        return value;
    }

    let serde_json::Value::Object(mut obj) = value else {
        return value;
    };
    if obj.len() != 1 || !obj.contains_key("result") {
        return serde_json::Value::Object(obj);
    }

    let inner = obj.remove("result").unwrap_or(serde_json::Value::Null);
    if array_root_path || fetch_text_for_raw(&inner) == extract_text(result) {
        inner
    } else {
        serde_json::json!({ "result": inner })
    }
}

fn format_tool_error(tool_name: &str, result: &CallToolResult) -> String {
    let text = extract_text(result);
    if text.is_empty() {
        format!("{tool_name} returned an error")
    } else {
        format!("{tool_name} returned an error: {text}")
    }
}
/// Generate TypeScript declarations from the visible catalog entries and
/// top-level tools, grouped by server prefix. Output looks like:
///
/// ```ts
/// declare namespace tools {
///   function bash(args: { command: string; ... }): Promise<any>;
///   function read(args: { file_path: string; ... }): Promise<any>;
///   namespace honeycomb {
///     function list_environments(args: { ... }): Promise<{ env_id: string }>;
///   }
/// }
/// ```
///
/// When a tool has an `output_schema`, the return type is derived from that
/// schema instead of the default `any`. Compose scripts see upstream `T`
/// directly; recovery metadata for cached sub-calls is exposed through
/// `ComposeOutput.sub_invocations`, not by wrapping each JS return value.
pub(crate) fn generate_dts(
    subject: &AuthSubject,
    sets: &[Arc<dyn SearchableToolSet>],
    top_level: &HashMap<String, Arc<dyn TopLevelTool>>,
) -> String {
    let mut top_fns: Vec<(String, String, String)> = Vec::new();
    for (name, tool) in top_level.iter() {
        if !tool.composable() || !tool.is_visible(subject) {
            continue;
        }
        let params_ts = json_schema_ts::schema_to_ts_params(tool.input_schema());
        let inner_ts = tool
            .inner_output_schema()
            .map(json_schema_ts::schema_to_ts)
            .unwrap_or_else(|| "any".to_string());
        top_fns.push((name.clone(), params_ts, inner_ts));
    }
    top_fns.sort_by(|a, b| a.0.cmp(&b.0));

    let mut namespaces: BTreeMap<String, Vec<(String, String, String)>> = BTreeMap::new();
    for set in sets.iter() {
        if !set.is_visible(subject) {
            continue;
        }
        let prefix = set.prefix().to_string();
        let tools_in_ns = namespaces.entry(prefix).or_default();
        for entry in set.tools() {
            let schema_val =
                serde_json::Value::Object(entry.description.input_schema.as_ref().clone());
            let params_ts = json_schema_ts::schema_to_ts_params(&schema_val);
            let inner_ts = entry
                .description
                .output_schema
                .as_ref()
                .map(|s| {
                    let schema_val = serde_json::Value::Object(s.as_ref().clone());
                    json_schema_ts::schema_to_ts(&schema_val)
                })
                .unwrap_or_else(|| "any".to_string());
            tools_in_ns.push((entry.name.clone(), params_ts, inner_ts));
        }
    }

    if namespaces.is_empty() && top_fns.is_empty() {
        return String::new();
    }

    let mut lines = vec!["declare namespace tools {".to_string()];

    for (name, params, ret) in &top_fns {
        lines.push(format!(
            "  function {name}(args: {{ {params} }}): Promise<{ret}>;"
        ));
    }

    for (ns, tools) in &namespaces {
        lines.push(format!("  namespace {ns} {{"));
        for (name, params, ret) in tools {
            lines.push(format!(
                "    function {name}(args: {{ {params} }}): Promise<{ret}>;"
            ));
        }
        lines.push("  }".to_string());
    }
    lines.push("}".to_string());
    lines.join("\n")
}

/// Append a "call `compose_types` first" suggestion to a dispatcher error so
/// the agent learns to fetch real signatures instead of re-guessing. The
/// prefix is derived from the requested tool name (everything before the
/// first underscore) so the hint scopes to the relevant namespace.
fn with_hint(tool_name: &str, raw: String) -> String {
    let prefix = tool_name.split('_').next().unwrap_or("*");
    format!(
        "{raw}\nHint: call compose_types({{tool_names:[\"{prefix}_*\"]}}) \
         to see available signatures."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tool(script_step: super::super::super::ScriptStepLimits) -> ComposeTool {
        ComposeTool::without_space_fs_for_test(
            Arc::new(RwLock::new(Vec::new())),
            Arc::new(RwLock::new(HashMap::new())),
            None,
            None,
            ComposeConfig {
                script_step,
                ..ComposeConfig::default()
            },
        )
    }

    #[test]
    fn for_script_falls_back_to_ceiling_when_unset() {
        let ceiling = super::super::super::ScriptStepLimits {
            max_tool_calls: 2000,
            timeout_ms: 1_800_000,
        };
        let scoped = test_tool(ceiling).for_script(None, None);
        assert_eq!(scoped.config.max_tool_calls, ceiling.max_tool_calls);
        assert_eq!(scoped.config.timeout_ms, ceiling.timeout_ms);
    }

    #[test]
    fn for_script_keeps_an_override_under_the_ceiling() {
        let ceiling = super::super::super::ScriptStepLimits {
            max_tool_calls: 2000,
            timeout_ms: 1_800_000,
        };
        let scoped = test_tool(ceiling).for_script(Some(10), Some(5_000));
        assert_eq!(scoped.config.max_tool_calls, 10);
        assert_eq!(scoped.config.timeout_ms, 5_000);
    }

    #[test]
    fn for_script_clamps_an_override_above_the_ceiling() {
        let ceiling = super::super::super::ScriptStepLimits {
            max_tool_calls: 2000,
            timeout_ms: 1_800_000,
        };
        let scoped = test_tool(ceiling).for_script(Some(999_999), Some(999_999_999));
        assert_eq!(scoped.config.max_tool_calls, ceiling.max_tool_calls);
        assert_eq!(scoped.config.timeout_ms, ceiling.timeout_ms);
    }

    struct CountingSet {
        entries: Vec<super::super::super::ToolSetEntry>,
        calls: Arc<Mutex<Vec<String>>>,
        visible: Arc<std::sync::atomic::AtomicBool>,
    }

    impl CountingSet {
        fn new(
            calls: Arc<Mutex<Vec<String>>>,
            visible: Arc<std::sync::atomic::AtomicBool>,
        ) -> Self {
            let entries = ["read", "write", "volatile"]
                .into_iter()
                .map(|n| {
                    let mut tool = rmcp::model::Tool::default();
                    tool.name = n.to_string().into();
                    super::super::super::ToolSetEntry {
                        name: n.to_string(),
                        description: tool,
                    }
                })
                .collect();
            Self {
                entries,
                calls,
                visible,
            }
        }
    }

    #[async_trait::async_trait]
    impl SearchableToolSet for CountingSet {
        fn name(&self) -> &str {
            "docs"
        }
        fn category(&self) -> &str {
            "test"
        }
        fn category_description(&self) -> &str {
            "test"
        }
        fn tools(&self) -> &[super::super::super::ToolSetEntry] {
            &self.entries
        }
        fn is_visible(&self, _subject: &AuthSubject) -> bool {
            self.visible.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn script_memoizable(&self, tool_name: &str) -> bool {
            tool_name == "read"
        }
        async fn call(
            &self,
            _subject: &AuthSubject,
            tool_name: &str,
            _arguments: Option<JsonObject>,
        ) -> Result<CallToolResult, ToolSetsError> {
            let n = {
                let mut calls = self.calls.lock().unwrap();
                calls.push(tool_name.to_string());
                calls.len()
            };
            let mut result = CallToolResult::success(Vec::new());
            result.structured_content = Some(serde_json::json!({ "n": n }));
            Ok(result)
        }
    }

    struct MemoHarness {
        dispatcher: CatalogDispatcher,
        calls: Arc<Mutex<Vec<String>>>,
        visible: Arc<std::sync::atomic::AtomicBool>,
    }

    fn memo_harness(max_memo_bytes: usize) -> MemoHarness {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let visible = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let set: Arc<dyn SearchableToolSet> =
            Arc::new(CountingSet::new(Arc::clone(&calls), Arc::clone(&visible)));
        let dispatcher = CatalogDispatcher {
            sets: Arc::new(RwLock::new(vec![set])),
            top_level: Arc::new(RwLock::new(HashMap::new())),
            subject: AuthSubject::Anonymous,
            audit: None,
            tool_caching: None,
            sub_invocations: Arc::new(Mutex::new(Vec::new())),
            memo: ScriptMemo::new(max_memo_bytes),
        };
        MemoHarness {
            dispatcher,
            calls,
            visible,
        }
    }

    use js_engine::ToolDispatcher as _;

    #[tokio::test]
    async fn memo_serves_identical_reads_from_one_dispatch() {
        let h = memo_harness(1 << 20);
        let a = h
            .dispatcher
            .call_tool(
                "docs_read",
                serde_json::json!({ "p": 1, "q": { "x": 1, "y": 2 } }),
            )
            .await
            .unwrap();
        let b = h
            .dispatcher
            .call_tool(
                "docs_read",
                serde_json::json!({ "q": { "y": 2, "x": 1 }, "p": 1 }),
            )
            .await
            .unwrap();
        assert_eq!(a, b);
        assert_eq!(h.calls.lock().unwrap().len(), 1);

        h.dispatcher
            .call_tool("docs_read", serde_json::json!({ "p": 2 }))
            .await
            .unwrap();
        assert_eq!(h.calls.lock().unwrap().len(), 2, "different args miss");
    }

    #[tokio::test]
    async fn memo_is_cleared_by_a_non_memoizable_call() {
        let h = memo_harness(1 << 20);
        let args = serde_json::json!({ "p": 1 });
        h.dispatcher
            .call_tool("docs_read", args.clone())
            .await
            .unwrap();
        h.dispatcher
            .call_tool("docs_write", args.clone())
            .await
            .unwrap();
        h.dispatcher.call_tool("docs_read", args).await.unwrap();
        assert_eq!(
            h.calls.lock().unwrap().as_slice(),
            ["read", "write", "read"]
        );
    }

    #[tokio::test]
    async fn memo_skips_tools_that_do_not_opt_in() {
        let h = memo_harness(1 << 20);
        let args = serde_json::json!({});
        h.dispatcher
            .call_tool("docs_volatile", args.clone())
            .await
            .unwrap();
        h.dispatcher.call_tool("docs_volatile", args).await.unwrap();
        assert_eq!(h.calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn memo_is_scoped_to_one_dispatcher() {
        let a = memo_harness(1 << 20);
        let b = memo_harness(1 << 20);
        let args = serde_json::json!({ "p": 1 });
        a.dispatcher
            .call_tool("docs_read", args.clone())
            .await
            .unwrap();
        b.dispatcher
            .call_tool("docs_read", args.clone())
            .await
            .unwrap();
        a.dispatcher
            .call_tool("docs_read", args.clone())
            .await
            .unwrap();
        assert_eq!(a.calls.lock().unwrap().len(), 1);
        assert_eq!(
            b.calls.lock().unwrap().len(),
            1,
            "b dispatched for itself instead of reusing a's memo"
        );
    }

    #[tokio::test]
    async fn memo_never_serves_a_tool_the_caller_can_no_longer_resolve() {
        let h = memo_harness(1 << 20);
        let args = serde_json::json!({ "p": 1 });
        h.dispatcher
            .call_tool("docs_read", args.clone())
            .await
            .unwrap();
        h.visible.store(false, std::sync::atomic::Ordering::SeqCst);
        let err = h
            .dispatcher
            .call_tool("docs_read", args)
            .await
            .expect_err("invisible tool must not be served from the memo");
        assert!(err.contains("Tool not found"), "{err}");
    }

    #[tokio::test]
    async fn memo_stops_inserting_past_the_byte_cap() {
        let h = memo_harness(1);
        let args = serde_json::json!({ "p": 1 });
        h.dispatcher
            .call_tool("docs_read", args.clone())
            .await
            .unwrap();
        h.dispatcher.call_tool("docs_read", args).await.unwrap();
        assert_eq!(h.calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn memo_hit_audit_context_carries_memo_hit_flag() {
        let seed = EventContext::current().data();
        let ctx = async {
            record_memo_hit_context(
                "compose > catalog: docs_read".into(),
                "docs_read",
                &serde_json::json!({ "p": 1 }),
            );
            Audit::collect_context().expect("context recorded")
        }
        .with_event_context(seed)
        .await;
        let metadata = ctx.metadata.expect("metadata");
        assert_eq!(metadata["memo_hit"], serde_json::json!(true));
        assert_eq!(metadata["tool_name"], "docs_read");
        assert_eq!(ctx.tokens_returned, Some(0));
        assert!(matches!(
            ctx.outcome,
            Some(crate::audit::primitives::InteractionOutcome::Success)
        ));
    }

    #[test]
    fn with_hint_appends_compose_types_suggestion() {
        let out = with_hint("concourse_list_builds", "Tool not found".to_string());
        assert!(out.contains("compose_types"));
        assert!(out.contains("concourse_*"));
    }

    #[test]
    fn tool_output_fetch_compose_value_unwraps_mcp_result_envelope() {
        let mut result =
            CallToolResult::success(vec![rmcp::model::Content::text("kube-system row")]);
        result.structured_content = Some(serde_json::json!({"result": "kube-system row"}));

        assert_eq!(
            tool_output_fetch_compose_value(&serde_json::json!({"path": "$"}), &result),
            serde_json::json!("kube-system row"),
        );
    }

    #[test]
    fn tool_output_fetch_compose_value_keeps_path_wrapped_objects() {
        let mut result = CallToolResult::success(Vec::new());
        result.structured_content = Some(serde_json::json!({"logs": "kube-system row"}));

        assert_eq!(
            tool_output_fetch_compose_value(&serde_json::json!({"path": "$.logs"}), &result),
            serde_json::json!({"logs": "kube-system row"}),
        );
    }

    #[test]
    fn tool_output_fetch_compose_value_keeps_real_result_path_wrapper() {
        let mut result = CallToolResult::success(vec![rmcp::model::Content::text("inner")]);
        result.structured_content = Some(serde_json::json!({"result": "inner"}));

        assert_eq!(
            tool_output_fetch_compose_value(&serde_json::json!({"path": "$.result"}), &result),
            serde_json::json!({"result": "inner"}),
        );
    }

    #[test]
    fn tool_output_fetch_compose_value_unwraps_array_root_path_wrapper() {
        let mut result =
            CallToolResult::success(vec![rmcp::model::Content::text(r#"{"body":"inner"}"#)]);
        result.structured_content = Some(serde_json::json!({
            "result": [{"body": "inner"}],
        }));

        assert_eq!(
            tool_output_fetch_compose_value(&serde_json::json!({"path": "$[2].body"}), &result),
            serde_json::json!([{"body": "inner"}]),
        );
    }

    #[test]
    fn output_schema_array_of_objects() {
        let schema = serde_json::json!({
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "build_id": { "type": "integer" },
                    "name": { "type": "string" }
                },
                "required": ["build_id", "name"]
            }
        });
        let ts = json_schema_ts::schema_to_ts(&schema);
        assert!(ts.contains("build_id: number"), "{ts}");
        assert!(ts.contains("name: string"), "{ts}");
        assert!(ts.ends_with("[]"), "{ts}");
    }

    #[test]
    fn output_schema_array_of_primitives() {
        let schema = serde_json::json!({
            "type": "array",
            "items": { "type": "string" }
        });
        assert_eq!(json_schema_ts::schema_to_ts(&schema), "string[]");
    }

    #[test]
    fn output_schema_flat_object_still_works() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "logs": { "type": "string" } },
            "required": ["logs"]
        });
        let ts = json_schema_ts::schema_to_ts(&schema);
        assert!(ts.contains("logs: string"), "{ts}");
    }

    #[test]
    fn end_to_end_schemars_to_ts() {
        use std::collections::HashMap;

        #[derive(serde::Serialize, schemars::JsonSchema)]
        #[serde(rename_all = "snake_case")]
        #[allow(dead_code)]
        enum Status {
            Pending,
            Done,
        }

        #[derive(serde::Serialize, schemars::JsonSchema)]
        #[allow(dead_code)]
        struct Sample {
            id: String,
            description: Option<String>,
            tags: Vec<String>,
            counters: HashMap<String, u32>,
            status: Status,
        }

        let schema = super::super::schema_for::<Sample>();
        let ts = json_schema_ts::schema_to_ts(&schema);

        assert!(ts.contains("id: string"), "{ts}");
        assert!(
            ts.contains("string | null") || ts.contains("null | string"),
            "{ts}"
        );
        assert!(ts.contains("tags: string[]"), "{ts}");
        assert!(ts.contains("Record<string, number>"), "{ts}");
        assert!(ts.contains("\"pending\""), "{ts}");
        assert!(ts.contains("\"done\""), "{ts}");
    }

    /// Project-admin tools (`agent`, `skill`, `workflow`, `sandbox`,
    /// `spaces`, `notes`, `log`) MUST be reachable from compose so admins
    /// can script-automate them. Stub `TopLevelTool` impls mirror the
    /// real tools' names and `composable() == true`; `generate_dts` then
    /// declares each one as a callable function on the `tools` namespace.
    #[test]
    fn admin_tools_appear_in_compose_dts() {
        struct StubAdminTool {
            name: &'static str,
            composable: bool,
        }

        #[async_trait::async_trait]
        impl TopLevelTool for StubAdminTool {
            fn name(&self) -> &str {
                self.name
            }
            fn description(&self) -> &str {
                "stub admin tool"
            }
            fn input_schema(&self) -> &serde_json::Value {
                static EMPTY: LazyLock<serde_json::Value> =
                    LazyLock::new(|| serde_json::json!({"type": "object"}));
                &EMPTY
            }
            fn composable(&self) -> bool {
                self.composable
            }
            async fn call(
                &self,
                _subject: &AuthSubject,
                _arguments: Option<JsonObject>,
            ) -> Result<CallToolResult, ToolSetsError> {
                unreachable!("stub: not invoked in this test")
            }
        }

        let mut top: HashMap<String, Arc<dyn TopLevelTool>> = HashMap::new();
        for name in [
            "agent", "skill", "workflow", "sandbox", "spaces", "notes", "log",
        ] {
            top.insert(
                name.to_string(),
                Arc::new(StubAdminTool {
                    name,
                    composable: true,
                }) as Arc<dyn TopLevelTool>,
            );
        }
        // A non-composable meta tool should NOT leak into dts.
        top.insert(
            "compose".to_string(),
            Arc::new(StubAdminTool {
                name: "compose",
                composable: false,
            }) as Arc<dyn TopLevelTool>,
        );

        let sets: Vec<Arc<dyn SearchableToolSet>> = Vec::new();
        let dts = generate_dts(&AuthSubject::Anonymous, &sets, &top);

        for name in [
            "agent", "skill", "workflow", "sandbox", "spaces", "notes", "log",
        ] {
            assert!(
                dts.contains(&format!("function {name}(")),
                "expected `function {name}(` in dts, got:\n{dts}"
            );
        }
        assert!(
            !dts.contains("function compose("),
            "non-composable tool leaked into dts:\n{dts}"
        );
    }

    /// `cache_envelope` merges `_recovery` into the ComposeOutput root
    /// when the walker elides. The schema is generated with
    /// `additionalProperties: false`, so the key must be declared up
    /// front or strict MCP clients reject every elided compose call.
    #[test]
    fn compose_output_schema_declares_recovery_properties() {
        let props = COMPOSE_OUTPUT_SCHEMA
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("compose outputSchema has properties");
        assert!(
            props.contains_key("_recovery"),
            "_recovery missing — strict validators reject elided compose"
        );
        assert!(
            !props.contains_key("_elided"),
            "_elided was a duplicate of _recovery.paths and is no longer emitted"
        );
        // It MUST stay optional — non-elided compose responses don't carry it.
        let required: Vec<&str> = COMPOSE_OUTPUT_SCHEMA
            .get("required")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        assert!(
            !required.contains(&"_recovery"),
            "_recovery must be optional"
        );
    }

    /// Strict MCP clients (e.g. Claude Code) reject boolean schemas inside
    /// `properties`. The dynamic `result` field must serialize as `{}`, not
    /// `true`.
    #[test]
    fn compose_output_schema_has_no_boolean_properties() {
        let props = COMPOSE_OUTPUT_SCHEMA
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("compose outputSchema has properties");
        for (name, schema) in props {
            assert!(
                !matches!(schema, serde_json::Value::Bool(_)),
                "property `{name}` is a boolean schema, MCP validators reject it"
            );
        }
    }
}
