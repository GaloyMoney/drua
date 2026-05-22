//! Execute JavaScript that chains multiple MCP tool calls in a single round trip.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::Duration;

use drua_tool_caching::{extract_text, fetch_text_for_raw, tool_result_value, ToolCaching};
use es_entity::context::{EventContext, WithEventContext};
use rmcp::model::{CallToolResult, JsonObject};
use serde::Deserialize;

use crate::audit::{Audit, InteractionType};
use crate::auth::AuthSubject;

use super::super::config::ComposeConfig;
use super::super::error::ToolSetsError;
use super::super::traits::{SearchableToolSet, TopLevelTool};
use super::{parse_params, schema_for};

#[derive(Deserialize, schemars::JsonSchema)]
struct ComposeParams {
    script: String,
}

pub struct ComposeTool {
    sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
    top_level: Arc<RwLock<HashMap<String, Arc<dyn TopLevelTool>>>>,
    audit: Option<Arc<Audit>>,
    tool_caching: Option<Arc<ToolCaching>>,
    config: ComposeConfig,
}

impl ComposeTool {
    pub fn new(
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
        }
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

static COMPOSE_OUTPUT_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<ComposeOutput>);

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
         — guessing leads to runtime errors that waste a round trip.\n\n\
         Example:\n```js\nconst envs = await tools.honeycomb.list_environments({});\n\
         const issues = await tools.github.list_issues({ repo: 'org/repo', state: 'open' });\n\
         const stale = issues.filter(i => Date.now() - Date.parse(i.updated_at) > 7*86400*1000);\n\
         return { envs, stale_issues: stale.map(i => i.number) };\n```"
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
        });

        let engine = js_engine::JsEngine::new()
            .with_max_tool_calls(self.config.max_tool_calls)
            .with_max_tool_result_bytes(self.config.max_tool_result_bytes)
            .with_max_return_bytes(self.config.max_return_bytes)
            .with_max_console_bytes(self.config.max_console_bytes)
            .with_memory_limit(self.config.memory_limit_bytes)
            .with_stack_limit(self.config.stack_limit_bytes);
        let result = engine
            .execute(&script, dispatcher, timeout)
            .await
            .map_err(|e| ToolSetsError::Compose(e.to_string()))?;

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

        let structured = serde_json::to_value(&out).expect("ComposeOutput serialization");
        let mut ctr = CallToolResult::success(Vec::new());
        ctr.structured_content = Some(structured);

        let ctr = match self.tool_caching.as_ref() {
            Some(tc) => {
                // Plain `cache()` — same path every other top-level tool
                // takes. We hand off an empty text channel; cache() fills
                // it from the structured payload (or replaces it with the
                // `<summary>+<recovery>` envelope when walking elides).
                tc.cache(subject, "compose", &recorded_args, ctr)
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
}

#[async_trait::async_trait]
impl js_engine::ToolDispatcher for CatalogDispatcher {
    async fn call_tool(
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

            return async move {
                Audit::record_action(action);
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
}

impl CatalogDispatcher {
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
            map.get(name)
                .filter(|t| t.composable() && t.is_visible(&self.subject))
                .cloned()
                .ok_or_else(|| with_hint(name, format!("Tool not found: {name}")))?
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

        async move {
            Audit::record_action(action);
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
    ) {
        let Some(tc) = self.tool_caching.as_ref() else {
            return;
        };
        let raw_size = serde_json::to_vec(&tool_result_value(raw))
            .map(|bytes| bytes.len() as u64)
            .unwrap_or_else(|_| extract_text(raw).len() as u64);
        let Ok(resp) = tc
            .persist_for_compose(&self.subject, tool_name, args, raw.clone())
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
            super::super::auto_parse_args::auto_parse_stringified_json_args(&mut a, &schema);
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
        super::super::auto_parse_args::auto_parse_stringified_json_args(
            &mut a,
            tool.input_schema(),
        );
        a
    });

    let result = tool
        .call(subject, inner_args)
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
