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
    /// Parse `original_text` into a `serde_json::Value`. Tried in order:
    ///
    /// 1. **JSON** — objects / arrays / scalars round-trip into their
    ///    native shape; quoted-JSON strings whose content is itself an
    ///    object/array literal are double-decoded (some upstreams wrap
    ///    their JSON body inside a JSON string in the text channel).
    /// 2. **YAML** — single-document YAML whose root is a mapping or
    ///    sequence (e.g. `kubectl get -o yaml` / `resources_get` output)
    ///    is converted to a JSON object/array so the structural walker
    ///    runs instead of flat line elision. Scalars, multi-document
    ///    streams, and any non-string-keyed mapping are rejected: YAML
    ///    folds free text (logs, diffs, tables) into plain scalars, and
    ///    accepting those would destroy their newlines and coerce values
    ///    (`port: 8080` → int). Anything YAML can't represent as JSON
    ///    falls through.
    /// 3. **String** — verbatim, including bare non-JSON/YAML text and
    ///    quoted scalars like `"hello"` or `"42"`.
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
            Err(_) => parse_yaml_single_doc(trimmed)
                .unwrap_or_else(|| Value::String(original_text.to_string())),
        };
        Self { root }
    }
}

/// Parse a single YAML document into a JSON value, rejecting anything that
/// is not a mapping or sequence at the root. See [`QueryStructure::new`]
/// step 2 for the rationale. Returns `None` for multi-document streams,
/// scalars/null, and any mapping that can't be represented as a JSON object
/// (non-string keys); the caller falls back to a verbatim string.
fn parse_yaml_single_doc(trimmed: &str) -> Option<Value> {
    // `serde_yaml::from_str` reads only the first document of a stream.
    // Use the Deserializer iterator to reject multi-document input up front
    // so a YAML stream doesn't get silently truncated to its first item.
    let mut docs = serde_yaml::Deserializer::from_str(trimmed);
    let first = docs.next()?;
    if docs.next().is_some() {
        return None;
    }
    let yaml = serde_yaml::Value::deserialize(first).ok()?;
    // Reject scalar/null roots: YAML folds free text (logs, diffs, tables)
    // into a single plain scalar, joining lines with spaces. Accepting
    // would destroy the newlines and coerce values. Only mappings and
    // sequences are structured documents worth walking structurally.
    // (`yaml_to_json` stays total for nested values, where a string is
    // legitimately a mapping value.)
    match &yaml {
        serde_yaml::Value::Mapping(_) | serde_yaml::Value::Sequence(_) => {}
        _ => return None,
    }
    yaml_to_json(yaml)
}

/// Convert a YAML value to JSON, returning `None` for any construct with no
/// JSON equivalent (non-string object keys; NaN/Infinity floats). Scalars and
/// null at the root are rejected by the caller's intent — but this helper is
/// total for nested values, so a `null` *inside* an accepted mapping becomes
/// `Value::Null` rather than rejecting the whole document.
fn yaml_to_json(yaml: serde_yaml::Value) -> Option<Value> {
    Some(match yaml {
        serde_yaml::Value::Null => Value::Null,
        serde_yaml::Value::Bool(b) => Value::Bool(b),
        serde_yaml::Value::Number(n) => yaml_number_to_json(n)?,
        serde_yaml::Value::String(s) => Value::String(s),
        serde_yaml::Value::Sequence(seq) => Value::Array(
            seq.into_iter()
                .map(yaml_to_json)
                .collect::<Option<Vec<_>>>()?,
        ),
        serde_yaml::Value::Mapping(m) => {
            let mut obj = serde_json::Map::with_capacity(m.len());
            for (k, v) in m {
                let key = yaml_scalar_key_to_string(k)?;
                obj.insert(key, yaml_to_json(v)?);
            }
            Value::Object(obj)
        }
        serde_yaml::Value::Tagged(t) => yaml_to_json(t.value)?,
    })
}

/// JSON object keys must be strings. Coerce a YAML scalar key to its string
/// form; reject mapping/sequence keys (valid in YAML, no JSON equivalent).
fn yaml_scalar_key_to_string(k: serde_yaml::Value) -> Option<String> {
    match k {
        serde_yaml::Value::String(s) => Some(s),
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        serde_yaml::Value::Null => Some("null".to_string()),
        serde_yaml::Value::Sequence(_) | serde_yaml::Value::Mapping(_) => None,
        serde_yaml::Value::Tagged(t) => yaml_scalar_key_to_string(t.value),
    }
}

fn yaml_number_to_json(n: serde_yaml::Number) -> Option<Value> {
    if let Some(i) = n.as_i64() {
        Some(Value::Number(i.into()))
    } else if let Some(u) = n.as_u64() {
        Some(Value::Number(u.into()))
    } else if let Some(f) = n.as_f64() {
        serde_json::Number::from_f64(f).map(Value::Number)
    } else {
        None
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
    /// True when this summary was produced by a tool that owns its
    /// envelope shape (`default_tool_caching() == false`, e.g. `compose`)
    /// — the live wire emits `T` verbatim via [`build_wire_envelope`],
    /// so summary replay must do the same to keep parity. `#[serde(default)]`
    /// keeps pre-existing persisted rows readable as wrap-mode.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub envelope_mode: bool,
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

    /// Variant of [`build_wire`] for tools that own their envelope shape
    /// and opt out of the `DruaToolResult` wrapper (i.e.
    /// `default_tool_caching() == false`, such as `compose`). The walker
    /// still operates on the full structured `T` and persists it for
    /// `tool_output_fetch` recovery, but the agent-facing structured
    /// channel keeps `T`'s own top-level shape — merging `_recovery` /
    /// `_elided` into `T`'s root object rather than nesting everything
    /// under a synthetic `result` key.
    ///
    /// This is what makes the returned `structuredContent` validate against
    /// such a tool's advertised `outputSchema` (e.g. `ComposeOutput`),
    /// which has no outer `{ result: ... }` wrapper. If `wire_result` is
    /// not an object (e.g. a tool returns an array or scalar root), fall
    /// back to [`build_wire`] so recovery metadata is never lost.
    pub fn build_wire_envelope(&self, invocation_id: ToolInvocationId) -> (CallToolResult, Value) {
        let envelope = self.build_envelope_text();
        let recovery = self.recovery_manifest(invocation_id);
        let mut root = self.wire_result.clone();
        match &mut root {
            Value::Object(map) => {
                map.insert(
                    "_recovery".to_string(),
                    serde_json::to_value(&recovery).unwrap(),
                );
                if !self.elided_paths.is_empty() {
                    map.insert(
                        "_elided".to_string(),
                        serde_json::json!({
                            "invocation_id": invocation_id.to_string(),
                            "paths": self.elided_paths.clone(),
                        }),
                    );
                }
            }
            _ => {
                // Non-object root — wrapping is the only way to attach
                // recovery metadata without losing the root value.
                return self.build_wire(invocation_id);
            }
        }
        let mut result = CallToolResult::success(vec![Content::text(envelope)]);
        result.structured_content = Some(root.clone());
        (result, root)
    }

    fn recovery_manifest(&self, invocation_id: ToolInvocationId) -> RecoveryManifest {
        RecoveryManifest {
            invocation_id: invocation_id.to_string(),
            root_kind: value_kind(&self.wire_result).to_string(),
            root_path: self.root_path.clone(),
            persisted_root: "persisted tool result root directly; catalog/sub-tool calls use upstream T, compose outer calls use ComposeOutput (JS return at $.result); not the outer {result: ...} MCP wire wrapper"
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
    pub root_path: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn root_is_string(s: &str) -> bool {
        matches!(QueryStructure::new(s).root, Value::String(_))
    }

    #[test]
    fn json_object_still_parses() {
        let qs = QueryStructure::new(r#"{"a": 1, "b": [2, 3]}"#);
        assert_eq!(qs.root, json!({"a": 1, "b": [2, 3]}));
    }

    #[test]
    fn quoted_json_inner_is_double_decoded() {
        let qs = QueryStructure::new(r#""{\"x\": 1}""#);
        assert_eq!(qs.root, json!({"x": 1}));
    }

    #[test]
    fn yaml_mapping_parses_to_object() {
        let qs = QueryStructure::new(
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: test\ndata:\n  key: value\n",
        );
        assert_eq!(
            qs.root,
            json!({
                "apiVersion": "v1",
                "kind": "ConfigMap",
                "metadata": {"name": "test"},
                "data": {"key": "value"},
            })
        );
    }

    #[test]
    fn yaml_sequence_parses_to_array() {
        let qs = QueryStructure::new("- one\n- two\n- 3\n");
        assert_eq!(qs.root, json!(["one", "two", 3]));
    }

    #[test]
    fn yaml_nested_numbers_and_bools_coerce() {
        let qs = QueryStructure::new("port: 8080\nready: true\nratio: 0.5\n");
        assert_eq!(qs.root, json!({"port": 8080, "ready": true, "ratio": 0.5}));
    }

    #[test]
    fn yaml_scalar_root_rejected_newlines_preserved() {
        // YAML folds free text into plain scalars (joining lines with
        // spaces); rejecting must return the ORIGINAL text verbatim so
        // line-mode elision still works downstream. A folded scalar
        // would collapse this to one space-joined line — assert that
        // does NOT happen.
        let log = "just a plain line of text\nand another line\n";
        let qs = QueryStructure::new(log);
        match qs.root {
            Value::String(ref s) => {
                assert_eq!(
                    s, log,
                    "original text must be preserved verbatim, not folded"
                );
                assert_eq!(s.matches('\n').count(), 2, "newlines must survive");
            }
            other => panic!("expected verbatim string, got {other:?}"),
        }
        assert!(matches!(
            QueryStructure::new("the build succeeded in 42 seconds.").root,
            Value::String(_)
        ));
    }

    #[test]
    fn yaml_null_root_rejected_as_string() {
        assert!(root_is_string("   \n  "));
    }

    #[test]
    fn yaml_multi_document_stream_rejected_as_string() {
        // `serde_yaml::from_str` silently truncates to the first document;
        // the iterator guard must reject so a stream isn't lost.
        assert!(root_is_string(
            "---\napiVersion: v1\nkind: A\n---\napiVersion: v1\nkind: B\n"
        ));
    }

    #[test]
    fn kubectl_style_log_lines_stay_a_string() {
        // Real log output: not `key: value`, so YAML folds to a scalar.
        let log = "INFO 2024-06-29 starting galoy-core\nlistening on :8080\nrequest handled\n";
        assert!(root_is_string(log));
    }

    #[test]
    fn bash_ls_output_stays_a_string() {
        let ls = "total 32\ndrwxr-xr-x  5 user staff  160 Jun 29 12:00 .\n-rw-r--r--  1 user staff 1024 Jun 29 12:00 file.txt\n";
        assert!(root_is_string(ls));
    }

    #[test]
    fn git_diff_hunk_stays_a_string() {
        let diff = "--- a/main.rs\n+++ b/main.rs\n@@ -1,3 +1,4 @@\n fn main() {\n-    old();\n+    new();\n }\n";
        assert!(root_is_string(diff));
    }

    #[test]
    fn kubectl_table_stays_a_string() {
        let table =
            "NAME              READY   STATUS    AGE\napp-7b8f-x2k      1/1     Running   3d\n";
        assert!(root_is_string(table));
    }

    #[test]
    fn json_takes_precedence_over_yaml() {
        // Valid JSON must not fall through to the YAML branch.
        let qs = QueryStructure::new(r#"{"a": 1}"#);
        assert_eq!(qs.root, json!({"a": 1}));
    }

    #[test]
    fn empty_and_whitespace_stay_string() {
        assert!(root_is_string(""));
        assert!(root_is_string("   \n\t  "));
    }

    #[test]
    fn numeric_yaml_key_coerced_to_string() {
        // Integer-keyed mapping: valid YAML. JSON has only string keys, so
        // coerce losslessly (`1` -> `"1"`) rather than rejecting the doc —
        // this is the standard JSON-library behaviour and keeps numeric-key
        // mappings structured for the walker.
        let qs = QueryStructure::new("1: one\n2: two\n");
        assert_eq!(qs.root, json!({"1": "one", "2": "two"}));
    }

    #[test]
    fn mapping_as_yaml_key_rejected_as_string() {
        // A mapping/sequence as a key has no JSON equivalent; reject the doc.
        assert!(root_is_string("? a: b\n: value\n"));
    }
}
