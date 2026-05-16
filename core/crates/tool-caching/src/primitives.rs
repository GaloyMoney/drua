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
///
/// `summary` is the value rendered inside `<summary>...</summary>` — for
/// concourse-style tools where a preprocessor advertises a primary text
/// path, this is the value at that path (e.g. the elided `logs` string).
/// `wire_result` is always the full walked structured tree — what goes
/// on the structured channel as `result` so the upstream schema validates.
///
/// `total_*` / `shown_*` describe what's in `<summary>` (the primary
/// view). Per-elision specifics for nested paths live in `elided_paths`
/// (each `ElidedPath` carries its own `total_*` / `shown_*`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallSummary {
    pub summary: serde_json::Value,
    pub wire_result: serde_json::Value,
    pub elided_paths: Vec<ElidedPath>,
    pub root_path: String,
    pub total_bytes: u64,
    pub shown_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_items: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shown_items: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_lines: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shown_lines: Option<u32>,
}

impl ToolCallSummary {
    /// Build the full agent-facing wire shape: text-channel envelope +
    /// structured-channel `{result, _elided?: {invocation_id, paths}}`
    /// payload. Both `cache()` (initial elision) and the `FetchQuery::Summary`
    /// replay path go through this single helper so the byte-identical
    /// guarantee between original-call and re-fetched summary can't drift.
    pub fn build_wire(&self, invocation_id: ToolInvocationId) -> (CallToolResult, Value) {
        let envelope = self.build_envelope_text();
        let mut obj = serde_json::Map::new();
        let recovery = self.recovery_manifest(invocation_id);
        obj.insert(
            "_recovery".to_string(),
            serde_json::to_value(&recovery).unwrap(),
        );
        if !self.elided_paths.is_empty() {
            obj.insert(
                "_elided".to_string(),
                serde_json::json!({
                    "invocation_id": invocation_id.to_string(),
                    "paths": self.elided_paths.clone(),
                }),
            );
        }
        obj.insert("result".to_string(), self.wire_result.clone());
        let structured = Value::Object(obj);
        let mut result = CallToolResult::success(vec![Content::text(envelope)]);
        result.structured_content = Some(structured.clone());
        (result, structured)
    }

    fn recovery_manifest(&self, invocation_id: ToolInvocationId) -> RecoveryManifest {
        RecoveryManifest {
            invocation_id: invocation_id.to_string(),
            root_kind: value_kind(&self.wire_result).to_string(),
            root_path: self.root_path.clone(),
            persisted_root: "upstream T directly; not the outer {result: ...} wire wrapper"
                .to_string(),
            paths: self.elided_paths.clone(),
            recommended_queries: self
                .elided_paths
                .iter()
                .map(|p| p.recover.clone())
                .collect(),
            sub_invocations: extract_sub_invocations(&self.wire_result),
        }
    }

    /// Build the `<summary>…</summary><recovery>…</recovery>` text-channel
    /// envelope from the walked summary. The structured channel is built
    /// separately by the caller (lib.rs) since its shape varies between
    /// the two public entry points.
    pub fn build_envelope_text(&self) -> String {
        let summary_text = match &self.summary {
            Value::String(s) => s.clone(),
            other => serde_json::to_string(other).unwrap_or_default(),
        };
        let mut envelope = String::new();
        let mut attrs = format!(
            "<summary path=\"{}\" total-bytes=\"{}\" shown-bytes=\"{}\"",
            self.root_path, self.total_bytes, self.shown_bytes,
        );
        if let (Some(total), Some(shown)) = (self.total_items, self.shown_items) {
            attrs.push_str(&format!(" total-items=\"{total}\" shown-items=\"{shown}\""));
        }
        if let (Some(total), Some(shown)) = (self.total_lines, self.shown_lines) {
            attrs.push_str(&format!(" total-lines=\"{total}\" shown-lines=\"{shown}\""));
        }
        envelope.push_str(&attrs);
        envelope.push_str(">\n");
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
        envelope
    }
}

/// Per-elision metadata surfaced inside `<recovery>` and persisted
/// alongside the upstream payload. Each elision point is self-describing:
/// `total_*` / `shown_*` for the dimensions that apply to the elided
/// segment (bytes always; lines for line-mode string elisions; items for
/// array truncations) plus `recover` — the verbatim `tool_output_fetch`
/// call template that retrieves the withheld portion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElidedPath {
    pub path: String,
    pub total_bytes: u64,
    pub shown_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_lines: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shown_lines: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_items: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shown_items: Option<u32>,
    pub recover: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryManifest {
    pub invocation_id: String,
    pub root_kind: String,
    pub root_path: String,
    pub persisted_root: String,
    pub paths: Vec<ElidedPath>,
    pub recommended_queries: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sub_invocations: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummaryFetchInfo {
    pub summary_envelope_bytes: u64,
    pub normal_fetch_limit_bytes: u64,
}

impl ElidedPath {
    pub(crate) fn render(&self) -> String {
        let mut attrs = format!(
            "  <elided path=\"{}\" total-bytes=\"{}\" shown-bytes=\"{}\"",
            self.path, self.total_bytes, self.shown_bytes,
        );
        if let (Some(total), Some(shown)) = (self.total_lines, self.shown_lines) {
            attrs.push_str(&format!(" total-lines=\"{total}\" shown-lines=\"{shown}\""));
        }
        if let (Some(total), Some(shown)) = (self.total_items, self.shown_items) {
            attrs.push_str(&format!(" total-items=\"{total}\" shown-items=\"{shown}\""));
        }
        let note = self
            .recover
            .get("note")
            .and_then(Value::as_str)
            .map(|note| format!("    note: {note}\n"))
            .unwrap_or_default();
        format!(
            "{attrs}>\n{note}    {}\n  </elided>\n",
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

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn extract_sub_invocations(root: &Value) -> Vec<Value> {
    root.get("sub_invocations")
        .and_then(Value::as_array)
        .map(|arr| arr.to_vec())
        .unwrap_or_default()
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
    pub summary_fetch_info: Option<SummaryFetchInfo>,
}
