//! Execute JavaScript that chains multiple MCP tool calls in a single round trip.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::Duration;

use drua_tool_caching::ToolCaching;
use es_entity::context::{EventContext, WithEventContext};
use rmcp::model::{CallToolResult, Content, JsonObject};
use serde::Deserialize;

use crate::audit::{Audit, InteractionType};
use crate::auth::AuthSubject;

use super::super::config::ComposeConfig;
use super::super::error::ToolSetsError;
use super::super::traits::{SearchableToolSet, TopLevelTool};
use super::liberal;
use super::{parse_params, schema_for};

#[derive(Deserialize, schemars::JsonSchema)]
struct ComposeParams {
    script: String,

    #[serde(default, deserialize_with = "liberal::deserialize_option_i64")]
    timeout_ms: Option<i64>,
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
    /// The JS script's return value. Curated when oversize; recoverable via `tool_output_fetch` using `result_invocation_id`.
    #[schemars(schema_with = "crate::toolset::any_json_schema")]
    result: serde_json::Value,
    /// Set when `result` was curated; pass to `tool_output_fetch` to recover the full JS return.
    #[serde(skip_serializing_if = "Option::is_none")]
    result_invocation_id: Option<uuid::Uuid>,
    /// Recoverable sub-tool calls; excludes passthrough, errored, and bypass-marked calls.
    sub_invocations: Vec<SubInvocation>,
    fetch_hint: String,
    console: Vec<String>,
    tool_calls: usize,
    execution_time_ms: u64,
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

    fn output_schema(&self) -> Option<&serde_json::Value> {
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

        let max_timeout = Duration::from_millis(self.config.max_timeout_ms);
        let default_timeout = Duration::from_millis(self.config.default_timeout_ms);
        let timeout = match params.timeout_ms {
            Some(ms) if ms > 0 => Duration::from_millis(ms as u64).min(max_timeout),
            _ => default_timeout,
        };

        Audit::record_action("compose".to_string());

        let recorded_args = serde_json::json!({
            "script": params.script,
            "timeout_ms": params.timeout_ms,
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

        // Cache the JS return value itself — if it's oversized the agent
        // gets a `result_invocation_id` to recover the full value via
        // `tool_output_fetch`. Builds a fake `CallToolResult` carrying
        // the value as JSON text so the existing pipeline can summarise it.
        let (curated_result, result_invocation_id) = match self.tool_caching.as_ref() {
            Some(tc) => {
                let pretty = serde_json::to_string_pretty(&result.value).unwrap_or_default();
                let mut ctr = CallToolResult::success(vec![Content::text(pretty)]);
                ctr.structured_content = Some(result.value.clone());
                let resp = tc
                    .maybe_summarize_and_cache(subject, "compose:result", &recorded_args, ctr)
                    .await?;
                let id = resp.invocation_id.map(uuid::Uuid::from);
                let curated = match id {
                    Some(_) => serde_json::Value::String(extract_text(&resp.result)),
                    None => result.value.clone(),
                };
                (curated, id)
            }
            None => (result.value.clone(), None),
        };

        let out = ComposeOutput {
            result: curated_result,
            result_invocation_id,
            sub_invocations: collected_sub_invocations,
            fetch_hint: COMPOSE_FETCH_HINT.to_string(),
            console: result.console_output.clone(),
            tool_calls: result.tool_calls_made,
            execution_time_ms: result.execution_time.as_millis() as u64,
        };

        let mut sections = Vec::new();
        let value_str =
            serde_json::to_string_pretty(&out.result).unwrap_or_else(|_| "null".to_string());
        sections.push(format!("=== Result ===\n{value_str}"));
        if !out.console.is_empty() {
            sections.push(format!("=== Console ===\n{}", out.console.join("\n")));
        }
        if !dts.is_empty() {
            sections.push(format!("=== Available Types ===\n{dts}"));
        }
        if !out.sub_invocations.is_empty() {
            let lines: Vec<String> = out
                .sub_invocations
                .iter()
                .map(|s| {
                    format!(
                        "  [{}] {} → {} ({} bytes raw / {} bytes summary, kind={})",
                        s.seq,
                        s.args_digest,
                        s.invocation_id,
                        s.raw_size_bytes,
                        s.summary_size_bytes,
                        s.kind,
                    )
                })
                .collect();
            sections.push(format!("=== Sub Invocations ===\n{}", lines.join("\n"),));
        }
        sections.push(format!(
            "=== Metadata ===\ntool_calls: {}\nexecution_time: {:?}\n{}",
            out.tool_calls,
            std::time::Duration::from_millis(out.execution_time_ms),
            out.result_invocation_id
                .map(|id| format!("result_invocation_id: {id}"))
                .unwrap_or_else(|| "result: verbatim (small enough)".to_string()),
        ));

        let text = sections.join("\n\n");
        let structured = serde_json::to_value(&out).expect("ComposeOutput serialization");
        let mut ctr = CallToolResult::success(vec![Content::text(text)]);
        ctr.structured_content = Some(structured);

        let ctr = match self.tool_caching.as_ref() {
            Some(tc) => {
                tc.maybe_summarize_and_cache(subject, "compose", &recorded_args, ctr)
                    .await?
                    .result
            }
            None => ctr,
        };
        Ok(ctr)
    }
}

const COMPOSE_FETCH_HINT: &str = "invocation_id is either `result_invocation_id` (full JS return) \
     or any `sub_invocations[].invocation_id` (specific sub-call's \
     persisted output) — see `tool_output_fetch` for the full call shape.";

/// Recovery metadata for one sub-tool dispatch inside a compose script.
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct SubInvocation {
    /// Order within the script (0-based, stable across re-runs).
    pub seq: u32,
    pub tool_name: String,
    /// `tool(key=value, ...)` truncated to 80 chars.
    pub args_digest: String,
    /// `tool_invocations.id` — pass to `tool_output_fetch`.
    pub invocation_id: uuid::Uuid,
    /// Summary kind discriminator (e.g. `concourse`, `structured_elision`, `generic`).
    pub kind: String,
    pub raw_size_bytes: u64,
    pub summary_size_bytes: u64,
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
        let bypass = !tool.default_tool_caching();

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
                    if !bypass {
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
        let raw_size = extract_text(raw).len() as u64;
        let Ok(resp) = tc
            .maybe_summarize_and_cache(&self.subject, tool_name, args, raw.clone())
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
        let summary_size = extract_text(&resp.result).len() as u64;
        let kind = sub_invocation_kind(&resp.elided_paths);
        let seq = self
            .sub_invocations
            .lock()
            .map(|guard| guard.len() as u32)
            .unwrap_or(0);
        if let Ok(mut guard) = self.sub_invocations.lock() {
            guard.push(SubInvocation {
                seq,
                tool_name: tool_name.to_string(),
                args_digest: format_args_digest(tool_name, args),
                invocation_id: uuid::Uuid::from(invocation_id),
                kind,
                raw_size_bytes: raw_size,
                summary_size_bytes: summary_size,
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

    let value = result_to_value(&result);
    Ok((result, value))
}

async fn run_top_level_call(
    tool: Arc<dyn TopLevelTool>,
    subject: &AuthSubject,
    args: serde_json::Value,
) -> Result<(CallToolResult, serde_json::Value), String> {
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

    let value = result_to_value(&result);
    Ok((result, value))
}

/// JS-engine view of a sub-call's result. Returns the upstream's actual
/// shape: a record from `structured_content` when set, otherwise the
/// upstream's text content parsed as JSON (or as a bare `Value::String` if
/// the text isn't JSON). No `{value|items, _shape}` envelope ever leaks
/// into JS.
fn result_to_value(result: &CallToolResult) -> serde_json::Value {
    if let Some(structured) = &result.structured_content {
        return structured.clone();
    }
    let text = extract_text(result);
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(v) => v,
        Err(_) => serde_json::Value::String(text),
    }
}

fn extract_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
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
/// schema instead of the default `any`.
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
        let return_ts = tool
            .output_schema()
            .map(json_schema_ts::schema_to_ts)
            .unwrap_or_else(|| "any".to_string());
        top_fns.push((name.clone(), params_ts, return_ts));
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
            let return_ts = entry
                .description
                .output_schema
                .as_ref()
                .map(|s| {
                    let schema_val = serde_json::Value::Object(s.as_ref().clone());
                    json_schema_ts::schema_to_ts(&schema_val)
                })
                .unwrap_or_else(|| "any".to_string());
            tools_in_ns.push((entry.name.clone(), params_ts, return_ts));
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
