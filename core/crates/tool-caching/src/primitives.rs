use rmcp::model::{CallToolResult, Content};
use serde::{Deserialize, Serialize};
use serde_json::Value;

es_entity::entity_id! {
    ToolInvocationId,
    ToolCallOwnerId,
}

/// Canonicalised view of a tool result the walker operates over.
pub struct QueryStructure {
    pub root: Value,
}

impl QueryStructure {
    /// Parse `original_text` into a `serde_json::Value`. JSON objects /
    /// arrays / scalars round-trip into their native shape. Quoted-JSON
    /// strings whose content is itself an object or array literal are
    /// double-decoded (some upstreams wrap their JSON body inside a JSON
    /// string in the text channel). Anything else — including bare
    /// non-JSON text and quoted scalars like `"hello"` or `"42"` — is
    /// kept verbatim as `Value::String`.
    pub fn new(original_text: &str) -> Self {
        let trimmed = original_text.trim();
        if trimmed.is_empty() {
            return Self {
                root: Value::String(original_text.to_string()),
            };
        }
        let root = match serde_json::from_str::<Value>(trimmed) {
            Ok(Value::String(inner)) => unwrap_quoted_json(inner),
            Ok(v) => v,
            Err(_) => Value::String(original_text.to_string()),
        };
        Self { root }
    }
}

/// If `inner` looks like a JSON object/array literal and parses cleanly
/// into one, return the unwrapped value; otherwise return `Value::String(inner)`.
fn unwrap_quoted_json(inner: String) -> Value {
    let trimmed = inner.trim_start();
    if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
        return Value::String(inner);
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(parsed @ (Value::Object(_) | Value::Array(_))) => parsed,
        _ => Value::String(inner),
    }
}

/// Output of `Walker::summarize`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallSummary {
    pub summary: serde_json::Value,
    pub elided_paths: Vec<ElidedPath>,
    pub root_path: String,
    pub original_bytes: u64,
}

/// Which channel-wrapping strategy `cache()` should apply.
///
/// All modes persist the upstream result for `tool_output_fetch` recovery.
/// They differ in what reaches the wire:
///
/// * [`WrapMode::Elide`] — top-level agent-facing. Walk and elide in place;
///   structured channel becomes `{result: T-elided, _elided?: M}` and the
///   text channel carries the `<summary>+<recovery>` envelope.
/// * [`WrapMode::Persist`] — compose sub-dispatch. Persist for recovery but
///   emit `{result: T verbatim}` on the structured channel — the JS engine
///   has its own size cap and we don't want sub-call results pre-summarised
///   before scripts touch them.
/// * [`WrapMode::TextOnly`] — compose's own output (and other tools whose
///   `default_tool_caching() == false`). Persist for recovery and emit the
///   `<summary>+<recovery>` text envelope, but leave `structured_content`
///   exactly as the upstream tool produced it. These tools own their own
///   structured shape (e.g. `ComposeOutput`) so the `DruaToolResult<T>`
///   wrapper would shadow their native fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrapMode {
    Elide,
    Persist,
    TextOnly,
}

impl ToolCallSummary {
    /// Build the wrapped `CallToolResult` the agent sees.
    ///
    /// Text channel: the `<summary>` + `<recovery>` envelope, identical for
    /// every mode. Tag style mirrors the kebab-case convention used inside
    /// chain markers (`<head bytes="…">` etc.) so the agent sees one
    /// consistent grammar.
    ///
    /// Structured channel varies by mode:
    /// * `Elide` — `{result: T-elided, _elided?: {invocation_id, paths}}`
    /// * `Persist` — `{result: T verbatim}` (no `_elided`)
    /// * `TextOnly` — `original_structured` verbatim (no wrapper at all)
    pub fn into_call_tool_result(
        self,
        mode: WrapMode,
        invocation_id: Option<ToolInvocationId>,
        original_structured: Option<serde_json::Value>,
        upstream_t: serde_json::Value,
    ) -> CallToolResult {
        let summary_text = match &self.summary {
            Value::String(s) => s.clone(),
            other => serde_json::to_string(other).unwrap_or_default(),
        };
        let mut envelope = String::new();
        envelope.push_str(&format!(
            "<summary path=\"{}\" original-bytes=\"{}\">\n",
            self.root_path, self.original_bytes,
        ));
        envelope.push_str(&summary_text);
        if !summary_text.ends_with('\n') {
            envelope.push('\n');
        }
        envelope.push_str("</summary>\n");
        envelope.push_str("<recovery>\n");
        for path in &self.elided_paths {
            envelope.push_str(&path.render());
        }
        envelope.push_str("</recovery>\n");

        let structured = match mode {
            WrapMode::Elide => {
                let mut obj = serde_json::Map::new();
                obj.insert("result".to_string(), self.summary);
                if !self.elided_paths.is_empty() {
                    let inv = invocation_id.map(|id| id.to_string()).unwrap_or_default();
                    obj.insert(
                        "_elided".to_string(),
                        serde_json::json!({
                            "invocation_id": inv,
                            "paths": self.elided_paths,
                        }),
                    );
                }
                Some(Value::Object(obj))
            }
            WrapMode::Persist => Some(serde_json::json!({ "result": upstream_t })),
            WrapMode::TextOnly => original_structured,
        };

        let mut result = CallToolResult::success(vec![Content::text(envelope)]);
        result.structured_content = structured;
        result
    }
}

/// Per-elision metadata surfaced inside `<recovery>` and persisted
/// alongside the upstream payload. `recover` is the verbatim
/// `tool_output_fetch` call template; `invocation_id` is the
/// `<this-invocation>` placeholder until persistence stamps the real id.
/// `lines` is `\n`-count of the original elided segment — agents reading
/// the envelope use it to gauge what byte ranges mean in row terms.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElidedPath {
    pub path: String,
    pub bytes: u64,
    /// `\n`-count of the original elided string. `None` for arrays /
    /// objects — they use `length` instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<u32>,
    /// Item count for arrays / key count for objects. `None` for
    /// strings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<u32>,
    /// For array truncation: how many leading items survive in the wrapped
    /// `result` (head-only — items `[0..head_count)` are present, items
    /// `[head_count..length)` are recoverable via `recover`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_count: Option<u32>,
    pub recover: Value,
}

impl ElidedPath {
    pub(crate) fn render(&self) -> String {
        let lines_attr = self
            .lines
            .map(|n| format!(" lines=\"{n}\""))
            .unwrap_or_default();
        let length_attr = self
            .length
            .map(|n| format!(" length=\"{n}\""))
            .unwrap_or_default();
        let head_attr = self
            .head_count
            .map(|n| format!(" head=\"{n}\""))
            .unwrap_or_default();
        format!(
            "  <elided path=\"{}\" bytes=\"{}\"{lines_attr}{length_attr}{head_attr}>\n    {}\n  </elided>\n",
            self.path,
            self.bytes,
            render_recover_call(&self.recover),
        )
    }
}

/// Render `{"tool": "name", "args_template": {k: v, …}}` as the literal
/// `name(k=<json v>, …)` call the agent will copy. Nested objects (e.g.
/// `query`) stay as compact JSON inside the kwarg value.
fn render_recover_call(recover: &Value) -> String {
    let tool = recover.get("tool").and_then(Value::as_str).unwrap_or("?");
    let Some(args) = recover.get("args_template").and_then(Value::as_object) else {
        return format!("{tool}()");
    };
    let kwargs = args
        .iter()
        .map(|(k, v)| format!("{k}={}", serde_json::to_string(v).unwrap_or_default()))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{tool}({kwargs})")
}

/// Returned by `ToolCaching::cache`. `elided_paths`
/// is the same list carried inside `result`'s envelope, surfaced
/// separately so `compose` can fold sub-call recovery info into
/// `ComposeOutput.sub_invocations` without re-parsing.
/// `invocation_id` is `Some` only when persistence ran (something was
/// elided); `None` means the upstream result passed through verbatim.
pub struct ToolCacheResponse {
    pub result: CallToolResult,
    pub elided_paths: Vec<ElidedPath>,
    pub invocation_id: Option<ToolInvocationId>,
}
