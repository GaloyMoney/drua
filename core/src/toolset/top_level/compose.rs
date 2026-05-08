//! `compose` — execute JavaScript that chains multiple MCP tool calls in a
//! single round trip. The script has access to a `tools` proxy; each
//! `tools.prefixed_name(args)` call dispatches to the backing catalog toolset
//! and is individually audit-logged.
//!
//! Includes TypeScript declaration generation from catalog JSON Schemas so
//! agents see typed APIs in the execution context.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, RwLock};
use std::time::Duration;

use es_entity::context::{EventContext, WithEventContext};
use rmcp::model::{CallToolResult, Content, JsonObject};
use serde::Deserialize;

use crate::audit::{Audit, InteractionType};
use crate::auth::AuthSubject;

use super::super::classifier::{
    Classification, ClassifierContext, ClassifierRegistry, GenericFallback, ResultClassifier,
    ToolResultSummary,
};
use super::super::config::ComposeConfig;
use super::super::error::ToolSetsError;
use super::super::tool_invocations::{PersistedClassification, ToolInvocations};
use super::super::traits::{SearchableToolSet, TopLevelTool};
use super::liberal;
use super::{parse_params, schema_for};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex as StdMutex;

#[derive(Deserialize, schemars::JsonSchema)]
struct ComposeParams {
    script: String,

    #[serde(default, deserialize_with = "liberal::deserialize_option_i64")]
    timeout_ms: Option<i64>,
}

pub struct ComposeTool {
    sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
    top_level: Arc<RwLock<HashMap<String, Arc<dyn TopLevelTool>>>>,
    /// `None` only in tests without a DB pool. Each sub-tool dispatch needs
    /// it to persist its own audit row with its own outcome (memory
    /// `019dfd6e`); without it the sub-tool action inherits the parent
    /// compose call's outcome.
    audit: Option<Arc<Audit>>,
    /// Same shape as `ToolSets::tool_invocations` — `None` in test paths.
    /// When set, sub-tool dispatches inside the JS engine route through
    /// `persist_classification` so each oversize sub-result lands in
    /// `tool_invocations` and surfaces in compose's `sub_invocations`
    /// directory. JS still receives the raw un-wrapped result.
    tool_invocations: Option<Arc<ToolInvocations>>,
    classifiers: Arc<ClassifierRegistry>,
    config: ComposeConfig,
}

impl ComposeTool {
    pub fn new(
        sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
        top_level: Arc<RwLock<HashMap<String, Arc<dyn TopLevelTool>>>>,
        audit: Option<Arc<Audit>>,
        tool_invocations: Option<Arc<ToolInvocations>>,
        classifiers: Arc<ClassifierRegistry>,
        config: ComposeConfig,
    ) -> Self {
        Self {
            sets,
            top_level,
            audit,
            tool_invocations,
            classifiers,
            config,
        }
    }
}

static COMPOSE_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<ComposeParams>);

#[derive(serde::Serialize, schemars::JsonSchema)]
struct ComposeOutput {
    /// The JS script's return value. Verbatim when small; under
    /// elision the walker's `kept` form (with sentinels for
    /// dropped branches). When elided, `result_invocation_id` is
    /// also present and the full return is recoverable via
    /// `tool_output_fetch`.
    #[schemars(schema_with = "crate::toolset::any_json_schema")]
    result: serde_json::Value,
    /// Set when `result` was curated by the walker. Pass to
    /// `tool_output_fetch` to recover the full JS return value.
    #[serde(skip_serializing_if = "Option::is_none")]
    result_invocation_id: Option<uuid::Uuid>,
    /// Each sub-tool call worth recovering. Excludes Passthrough
    /// (under-threshold) sub-calls, errored calls, and bypass-marked
    /// calls — entries listed here always have a valid `invocation_id`
    /// the agent can fetch.
    sub_invocations: Vec<SubInvocation>,
    /// Single fetch hint covering both `result_invocation_id` and any
    /// `sub_invocations[].invocation_id`. Mirrors the universal
    /// pipeline's hint shape so the agent learns one rule.
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

    fn bypass_universal_pipeline(&self) -> bool {
        // Compose owns its own envelope shape — richer than the
        // standard `{invocation_id, summary, fetch_hint}` because it
        // carries `result_invocation_id` (recovery for the JS return
        // value) AND `sub_invocations[]` (recovery handles for each
        // sub-tool call the script made). Letting the dispatcher
        // re-wrap compose's structured_content with the standard
        // envelope would clobber the `sub_invocations` directory.
        true
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

        // Snapshot the agent-supplied params for persistence — the
        // `tool_invocations.args` JSONB column is otherwise an opaque
        // `{}` for compose rows since the dts-prepended `script` and
        // `params` itself both get consumed below.
        let recorded_args = serde_json::json!({
            "script": params.script,
            "timeout_ms": params.timeout_ms,
        });

        let dts = {
            let sets = self.sets.read().expect("toolset lock poisoned");
            let top = self.top_level.read().expect("top_level lock poisoned");
            generate_dts(subject, &sets, &top)
        };

        // Prepend type declarations as a JS block comment for error diagnostics.
        let script = if dts.is_empty() {
            params.script
        } else {
            format!("/*\n{dts}*/\n{}", params.script)
        };

        let sub_invocations = Arc::new(StdMutex::new(Vec::<SubInvocation>::new()));
        let seq_counter = Arc::new(AtomicU32::new(0));
        let dispatcher = Arc::new(CatalogDispatcher {
            sets: Arc::clone(&self.sets),
            top_level: Arc::clone(&self.top_level),
            subject: subject.clone(),
            audit: self.audit.clone(),
            tool_invocations: self.tool_invocations.clone(),
            classifiers: Arc::clone(&self.classifiers),
            sub_invocations: Arc::clone(&sub_invocations),
            seq_counter: Arc::clone(&seq_counter),
        });

        let engine = js_engine::JsEngine::new()
            .with_max_tool_calls(self.config.max_tool_calls)
            .with_max_tool_result_bytes(self.config.max_tool_result_bytes)
            .with_max_return_bytes(self.config.max_return_bytes)
            .with_max_console_bytes(self.config.max_console_bytes)
            .with_memory_limit(self.config.memory_limit_bytes)
            .with_stack_limit(self.config.stack_limit_bytes);
        let started_at = chrono::Utc::now();
        let result = engine
            .execute(&script, dispatcher, timeout)
            .await
            .map_err(|e| ToolSetsError::Compose(e.to_string()))?;

        // Self-curation: run the unified walker over the JS return
        // value with the SAME threshold every other classifier uses.
        // Below threshold → result is the verbatim Value, no
        // result_invocation_id. Above threshold → walker produces
        // either StructuredElision (kept Value with sentinels) or
        // Passthrough (when elision wouldn't shrink). When elided,
        // persist the full return so the agent can fetch it.
        let walker_classifier = GenericFallback::default();
        let walker_input = build_walker_input(&result.value);
        let classifiers_for_region = Arc::clone(&self.classifiers);
        let classify_region = |region: &str| classifiers_for_region.classify_region(region);
        let classification = walker_classifier
            .classify(&ClassifierContext {
                tool_name: "compose",
                args: &recorded_args,
                raw: &walker_input,
                exit_code: None,
                classify_region: &classify_region,
            })
            .unwrap_or_else(|_| Classification {
                summary: ToolResultSummary::Passthrough {
                    value: result.value.clone(),
                },
                canonical_text: serde_json::to_string(&result.value).unwrap_or_default(),
            });

        let (curated_result, result_invocation_id) =
            match (classification.summary, self.tool_invocations.as_ref()) {
                (ToolResultSummary::Passthrough { value }, _) => (value, None),
                (summary, Some(invocations)) => {
                    let canonical_text = classification.canonical_text;
                    let curated = render_curated_result(&summary);
                    // Compose's own "structured_content" is the JS
                    // return value — exactly what the script meant to
                    // produce. Persist that as `original_structured`
                    // so a future fetch of `result_invocation_id`
                    // returns the same Value the script returned,
                    // not the curated/elided form.
                    let original_structured = Some(result.value.clone());
                    let persisted = invocations
                        .persist_classification(
                            subject,
                            "compose",
                            &recorded_args,
                            Classification {
                                summary,
                                canonical_text,
                            },
                            original_structured,
                            result.execution_time.as_millis() as u64,
                            started_at,
                        )
                        .await;
                    let invocation_id = persisted.map(|p| uuid::Uuid::from(p.invocation_id));
                    (curated, invocation_id)
                }
                (summary, None) => {
                    // No PG wired (test path) — surface the curated
                    // form so the agent still sees the elision shape,
                    // just without a recovery handle.
                    (render_curated_result(&summary), None)
                }
            };

        let sub_invocations = sub_invocations
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();

        let out = ComposeOutput {
            result: curated_result,
            result_invocation_id,
            sub_invocations,
            fetch_hint: COMPOSE_FETCH_HINT.to_string(),
            console: result.console_output.clone(),
            tool_calls: result.tool_calls_made,
            execution_time_ms: result.execution_time.as_millis() as u64,
        };

        // Prose rendering for the model's text content. Keep the
        // legacy `=== Result ===` / `=== Console ===` / `=== Metadata ===`
        // sections so existing agents reading the text form aren't
        // broken; add a `=== Sub Invocations ===` section when there's
        // anything to surface.
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
        Ok(ctr)
    }
}

/// Wrap a JS return Value as a `CallToolResult` so the existing
/// classifier surface can walk it. structured_content carries the
/// Value verbatim; content[].text is set to the compact serialized
/// form so any text-extraction fallback still works.
fn build_walker_input(value: &serde_json::Value) -> CallToolResult {
    let text = serde_json::to_string(value).unwrap_or_default();
    let mut ctr = CallToolResult::success(vec![Content::text(text)]);
    ctr.structured_content = Some(value.clone());
    ctr
}

/// For elided/non-Passthrough summaries, surface the walker's `kept`
/// view inline as compose's `result` field (so the agent still sees
/// data, just curated). Concourse / future typed shapes fall back
/// to JSON-stringifying the entire summary — they won't be the
/// shape compose's JS engine produces anyway.
fn render_curated_result(summary: &ToolResultSummary) -> serde_json::Value {
    match summary {
        ToolResultSummary::Passthrough { value } => value.clone(),
        ToolResultSummary::StructuredElision { kept, .. } => kept.clone(),
        ToolResultSummary::ConcourseLogs(s) => {
            serde_json::to_value(s).unwrap_or(serde_json::Value::Null)
        }
    }
}

/// Compose-specific disambiguator. The `tool_output_fetch` call
/// shape itself lives in that tool's `description()` — agents have
/// it via the catalog and don't need it duplicated here. What's
/// unique to compose is *which* invocation id to pass: compose
/// emits both `result_invocation_id` (the full JS return) and a
/// per-sub-call `sub_invocations[].invocation_id` directory.
const COMPOSE_FETCH_HINT: &str = "invocation_id is either `result_invocation_id` (full JS return) \
     or any `sub_invocations[].invocation_id` (specific sub-call's \
     persisted output) — see `tool_output_fetch` for the full call shape.";

/// One sub-tool dispatch's worth of recovery metadata. Accumulated by
/// `CatalogDispatcher` while the JS script runs; surfaced in compose's
/// final `structured_content.sub_invocations` directory so the agent
/// can `tool_output_fetch(invocation_id, ...)` on any specific
/// sub-call's persisted output.
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct SubInvocation {
    /// Order within the script — `seq=0` is the first `tools.foo()`
    /// call, `seq=1` the second, etc. Stable across re-runs of the
    /// same script.
    pub seq: u32,
    pub tool_name: String,
    /// Short human-readable identifier of the call. Format
    /// `tool(key=value, ...)` truncated to 80 chars; lets agents pick
    /// by call subject rather than by index.
    pub args_digest: String,
    /// `tool_invocations.id` — pass to `tool_output_fetch` to recover
    /// elided detail.
    pub invocation_id: uuid::Uuid,
    /// `summary.kind` discriminator (e.g. `"concourse"`,
    /// `"structured_elision"`, `"generic"`).
    pub kind: String,
    pub raw_size_bytes: u64,
    pub summary_size_bytes: u64,
}

/// Bridges the JS engine's [`js_engine::ToolDispatcher`] trait to both the
/// catalog (SearchableToolSet) dispatch path and the top-level tool registry,
/// preserving visibility filtering, audit logging, and output filtering.
struct CatalogDispatcher {
    sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
    top_level: Arc<RwLock<HashMap<String, Arc<dyn TopLevelTool>>>>,
    subject: AuthSubject,
    audit: Option<Arc<Audit>>,
    tool_invocations: Option<Arc<ToolInvocations>>,
    classifiers: Arc<ClassifierRegistry>,
    /// Accumulator threaded through every sub-tool call. Compose reads
    /// this after the JS engine finishes and emits the entries as
    /// `structured_content.sub_invocations`. `Mutex` (sync, not async)
    /// because mutations are short and uncontended — the JS engine
    /// dispatches sub-calls sequentially.
    sub_invocations: Arc<StdMutex<Vec<SubInvocation>>>,
    seq_counter: Arc<AtomicU32>,
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
                        // Run the universal pipeline as a side-effect.
                        // JS still gets `value`; persist + accumulate
                        // for the sub_invocations directory.
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
    /// Same logic as `CallCatalogTool::find_set()`.
    fn find_set(
        &self,
        prefixed_name: &str,
    ) -> Result<(Arc<dyn SearchableToolSet>, String), ToolSetsError> {
        let sets = self.sets.read().expect("toolset lock poisoned");
        for set in sets.iter() {
            if !set.is_visible(&self.subject) {
                continue;
            }
            let prefix = format!("{}_", set.prefix());
            if let Some(tool_name) = prefixed_name.strip_prefix(&prefix) {
                if set.tools().iter().any(|t| t.name == tool_name) {
                    return Ok((Arc::clone(set), tool_name.to_string()));
                }
            }
        }
        Err(ToolSetsError::ToolNotFound(prefixed_name.to_string()))
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
        let bypass = tool.bypass_universal_pipeline();

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

    /// Lightweight clone of the persistence-relevant fields. Used to
    /// hand a borrow of the persistence machinery into the per-call
    /// async block without holding a `&self` across the await.
    fn clone_for_persistence(&self) -> CatalogDispatcherShared {
        CatalogDispatcherShared {
            subject: self.subject.clone(),
            tool_invocations: self.tool_invocations.clone(),
            classifiers: Arc::clone(&self.classifiers),
            sub_invocations: Arc::clone(&self.sub_invocations),
            seq_counter: Arc::clone(&self.seq_counter),
        }
    }
}

/// Persistence-side view of [`CatalogDispatcher`] — fields the
/// per-call async block needs to record a `SubInvocation`. Cloned
/// once per dispatch so the closure can move it without holding a
/// borrow on the dispatcher.
#[derive(Clone)]
struct CatalogDispatcherShared {
    subject: AuthSubject,
    tool_invocations: Option<Arc<ToolInvocations>>,
    classifiers: Arc<ClassifierRegistry>,
    sub_invocations: Arc<StdMutex<Vec<SubInvocation>>>,
    seq_counter: Arc<AtomicU32>,
}

impl CatalogDispatcherShared {
    /// Run the universal pipeline as a side-effect. Skipped when:
    ///   - no `ToolInvocations` is wired (test paths)
    ///   - the subject doesn't yield an owner (`Anonymous`,
    ///     `WorkflowExecutor`)
    ///   - the classifier returned `Passthrough` (no recoverable
    ///     detail; agent already has every byte via the JS-facing
    ///     `Value`)
    ///   - persistence itself failed (logged inside
    ///     `persist_classification`)
    async fn maybe_persist_sub_invocation(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        raw: &CallToolResult,
        duration_ms: u64,
        started_at: chrono::DateTime<chrono::Utc>,
    ) {
        let Some(invocations) = self.tool_invocations.as_ref() else {
            return;
        };
        let classifiers = Arc::clone(&self.classifiers);
        let classify_region = |region: &str| classifiers.classify_region(region);
        let classification = self.classifiers.classify(&ClassifierContext {
            tool_name,
            args,
            raw,
            exit_code: None,
            classify_region: &classify_region,
        });
        if classification.summary.is_passthrough() {
            return;
        }
        let summary_value_size = serde_json::to_string(
            &serde_json::to_value(&classification.summary).unwrap_or_default(),
        )
        .map(|s| s.len() as u64)
        .unwrap_or(0);

        let Some(persisted): Option<PersistedClassification> = invocations
            .persist_classification(
                &self.subject,
                tool_name,
                args,
                classification,
                raw.structured_content.clone(),
                duration_ms,
                started_at,
            )
            .await
        else {
            return;
        };

        let seq = self.seq_counter.fetch_add(1, Ordering::Relaxed);
        let entry = SubInvocation {
            seq,
            tool_name: tool_name.to_string(),
            args_digest: args_digest(tool_name, args),
            invocation_id: persisted.invocation_id.into(),
            kind: persisted.summary.kind().to_string(),
            raw_size_bytes: persisted.raw_size_bytes,
            summary_size_bytes: summary_value_size,
        };
        if let Ok(mut guard) = self.sub_invocations.lock() {
            guard.push(entry);
        }
    }
}

/// Format `tool(key1=value1, key2=value2)` truncated to ~80 chars.
/// Reads better than a raw uuid for the agent scanning
/// `sub_invocations`.
fn args_digest(tool_name: &str, args: &serde_json::Value) -> String {
    let mut out = String::from(tool_name);
    out.push('(');
    if let serde_json::Value::Object(obj) = args {
        let mut first = true;
        for (k, v) in obj.iter().take(4) {
            if !first {
                out.push_str(", ");
            }
            first = false;
            let v_str: String = match v {
                serde_json::Value::String(s) => s.chars().take(20).collect(),
                other => other.to_string().chars().take(20).collect(),
            };
            out.push_str(k);
            out.push('=');
            out.push_str(&v_str);
        }
    }
    out.push(')');
    out.chars().take(80).collect()
}

/// Inner dispatch for a [`SearchableToolSet`] — returns both the raw
/// `CallToolResult` (for classifier + `tool_output_fetch` recovery)
/// and the JS-facing `Value`. No pre-classification trimming: the
/// dispatcher's universal pipeline handles oversize results, and
/// compose JS scripts get the full shape so they can filter /
/// cross-reference in JS (compose's value proposition).
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

    let result = set
        .call(subject, tool_name, inner_args)
        .await
        .map_err(|e| e.to_string())?;

    let value = result_to_value(&result);
    Ok((result, value))
}

/// Inner dispatch for a [`TopLevelTool`] — same shape as
/// [`run_searchable_call`].
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

    let result = tool
        .call(subject, inner_args)
        .await
        .map_err(|e| e.to_string())?;

    let value = result_to_value(&result);
    Ok((result, value))
}

/// Convert a `CallToolResult` to the `Value` shape JS expects:
/// structured_content wins; falls back to JSON-parsed text; falls
/// back further to `Value::String(text)`. Same logic both inner
/// dispatch paths used inline before; extracted so the new
/// `(CallToolResult, Value)` return type doesn't duplicate it.
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
