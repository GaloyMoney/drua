//! Recovery side of tool-caching: given a stored invocation, navigate
//! a json-path and optionally slice the resolved value.

use rmcp::model::{CallToolResult, Content};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ToolCachingError;
use crate::repo::StoredInvocation;

/// Slice operation applied at the resolved json-path. When absent, the
/// whole value at the path is returned. Mirrors the recovery template
/// the walker emits.
///
/// `offset` is signed so callers can use Python/JS slice semantics:
/// a negative offset counts from the end of the value (`-N` ⇒ last N).
/// Out-of-range offsets clamp to 0 / total rather than erroring.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum FetchQuery {
    /// UTF-8 safe byte window on a string-typed value at `path`. `offset`
    /// may be negative to count from the end of the string. If the
    /// requested byte boundaries land inside a UTF-8 codepoint, they are
    /// moved inward to the nearest valid character boundaries instead of
    /// failing the recovery request.
    Range { offset: i64, len: usize },
    /// Line range on a string-typed value at `path`. `offset` is the
    /// zero-indexed first line to return; `len` is the line count.
    /// `offset` may be negative to count from the end (e.g.
    /// `offset:-80, len:80` returns the last 80 lines).
    Lines { offset: i64, len: usize },
    /// Item range on an array-typed value at `path`. Returns the slice
    /// `arr[offset..offset+len]` as a `Value::Array`. `offset` may be
    /// negative to count from the end of the array.
    JsonArraySlice { offset: i64, len: usize },
    /// Return the persisted curated `<summary>+<recovery>` envelope —
    /// the same view the agent would have seen from a top-level
    /// `call_tool` invocation. Useful for inspecting a compose
    /// sub-invocation's curated form (JS scripts see verbatim T; the
    /// agent reading `sub_invocations[i].invocation_id` post-hoc can
    /// fetch the summary view here). `path` is ignored — the summary
    /// is always the root view. Summary recovery bypasses the normal
    /// fetch response cap; callers that advertise summary recovery
    /// should expose the envelope size before this fetch is attempted.
    Summary,
}

/// Translate a possibly-negative offset to a clamped `[0, total]` index.
/// Negative offsets count from the end (Python/JS slice semantics):
/// `-N` resolves to `total - N` (clamped to 0 when `N > total`).
fn resolve_offset(offset: i64, total: usize) -> usize {
    if offset >= 0 {
        (offset as usize).min(total)
    } else {
        let abs = offset.unsigned_abs() as usize;
        total.saturating_sub(abs)
    }
}

impl FetchQuery {
    fn apply(&self, value: Value) -> Result<Value, ToolCachingError> {
        match self {
            FetchQuery::Range { offset, len } => {
                let s = value.as_str().ok_or_else(|| {
                    ToolCachingError::InvalidPath(
                        "range query requires a string at the resolved path".into(),
                    )
                })?;
                let (start, end) = resolve_utf8_byte_window(s, *offset, *len);
                let slice = &s[start..end];
                Ok(Value::String(slice.to_string()))
            }
            FetchQuery::Lines { offset, len } => {
                let s = value.as_str().ok_or_else(|| {
                    ToolCachingError::InvalidPath(
                        "lines query requires a string at the resolved path".into(),
                    )
                })?;
                let all: Vec<&str> = s.lines().collect();
                let start = resolve_offset(*offset, all.len());
                let end = start.saturating_add(*len).min(all.len());
                Ok(Value::String(all[start..end].join("\n")))
            }
            FetchQuery::JsonArraySlice { offset, len } => {
                let arr = value.as_array().ok_or_else(|| {
                    ToolCachingError::InvalidPath(
                        "json_array_slice query requires an array at the resolved path".into(),
                    )
                })?;
                let start = resolve_offset(*offset, arr.len());
                let end = start.saturating_add(*len).min(arr.len());
                Ok(Value::Array(arr[start..end].to_vec()))
            }
            // Summary is short-circuited in `StoredInvocation::query` —
            // it doesn't fit the slice-at-resolved-path pattern.
            FetchQuery::Summary => Err(ToolCachingError::InvalidPath(
                "summary query is handled at the invocation level".into(),
            )),
        }
    }
}

fn resolve_utf8_byte_window(s: &str, offset: i64, len: usize) -> (usize, usize) {
    let requested_start = resolve_offset(offset, s.len());
    resolve_utf8_byte_window_usize(s, requested_start, len)
}

pub(crate) fn resolve_utf8_byte_window_usize(s: &str, offset: usize, len: usize) -> (usize, usize) {
    let requested_start = offset.min(s.len());
    let requested_end = requested_start.saturating_add(len).min(s.len());
    let start = ceil_char_boundary(s, requested_start);
    let end = floor_char_boundary(s, requested_end).max(start);
    (start, end)
}

pub(crate) fn floor_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

pub(crate) fn ceil_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// What `ToolCaching::fetch` hands back to the caller. `result` is the
/// agent-facing `CallToolResult`; `structured` is the path-wrapped json
/// value (e.g. `{"a": {"b": <slice>}}` for `path="$.a.b"`), surfaced
/// separately so the dispatch layer / compose can consume it without
/// re-parsing the text channel.
pub struct FetchResult {
    pub result: CallToolResult,
    pub structured: Value,
}

impl StoredInvocation {
    /// Resolve `path` against the stored root and slice with `query`.
    /// Wraps the result back at `path` so the response shape mirrors
    /// the caller's request (`$.foo[2]` → `{"foo": [X]}`). Responses
    /// over `max_bytes` (text channel size) are rejected.
    pub fn query(
        &self,
        path: &str,
        query: Option<&FetchQuery>,
        max_bytes: usize,
    ) -> Result<FetchResult, ToolCachingError> {
        // `Summary` short-circuits navigation — return the stored
        // envelope and its structured wrapper, regardless of `path`.
        if matches!(query, Some(FetchQuery::Summary)) {
            return self.summary_view(max_bytes);
        }
        let resolved = self.navigate(path)?.clone();
        let sliced = match query {
            Some(q) => q.apply(resolved)?,
            None => resolved,
        };
        // Text channel shows the resolved value raw — strings stay
        // human-readable instead of becoming JSON-escaped `"..."`.
        let text = fetch_text_for_raw(&sliced);
        let wrapped = wrap_at_path(path, sliced)?;
        if text.len() > max_bytes {
            return Err(ToolCachingError::FetchResponseTooLarge {
                size: text.len(),
                max: max_bytes,
                hint: self.too_large_hint(path, max_bytes),
            });
        }
        Ok(FetchResult {
            result: CallToolResult::success(vec![Content::text(text)]),
            structured: wrapped,
        })
    }

    /// Replay the curated `<summary>+<recovery>` envelope from the
    /// persisted `ToolCallSummary`. Summary mode is the recovery escape
    /// hatch, so it bypasses the normal fetch-size cap.
    fn summary_view(&self, _max_bytes: usize) -> Result<FetchResult, ToolCachingError> {
        let (result, structured) = self.summary.build_wire(self.id);
        Ok(FetchResult { result, structured })
    }

    /// Tail-appended hint for `FetchResponseTooLarge`. Always nudges
    /// `mode:'summary'`; on a compose root (`sub_invocations` array at
    /// `$`), names up to three drill-down invocation_ids the agent can
    /// fetch directly instead of re-slicing the aggregate body.
    fn too_large_hint(&self, path: &str, max_bytes: usize) -> String {
        let summary_envelope_bytes = self.summary.build_envelope_text().len();
        let mut parts = vec![format!(
            ". Try `query: {{mode: \"summary\"}}` for the curated envelope \
             (summary_envelope_bytes: {summary_envelope_bytes}, \
             normal_fetch_limit_bytes: {max_bytes}; summary mode bypasses that cap)"
        )];
        let touches_result = path == "$" || path == "$.result" || path.starts_with("$.result.");
        if touches_result {
            if let Some(arr) = self
                .query_structure
                .root
                .get("sub_invocations")
                .and_then(Value::as_array)
            {
                let ids: Vec<String> = arr
                    .iter()
                    .take(3)
                    .filter_map(|s| {
                        let id = s.get("invocation_id").and_then(Value::as_str)?;
                        let tool = s.get("tool_name").and_then(Value::as_str).unwrap_or("?");
                        Some(format!("{tool}={id}"))
                    })
                    .collect();
                if !ids.is_empty() {
                    let more = if arr.len() > ids.len() {
                        format!(" (+{} more in `$.sub_invocations`)", arr.len() - ids.len())
                    } else {
                        String::new()
                    };
                    parts.push(format!(
                        ". Or drill into a sub-invocation directly via tool_output_fetch: {}{}",
                        ids.join(", "),
                        more,
                    ));
                }
            }
        }
        parts.join("")
    }

    fn navigate(&self, path: &str) -> Result<&Value, ToolCachingError> {
        let segments = parse_path(path)?;
        let mut cur = &self.query_structure.root;
        for seg in &segments {
            cur = match seg {
                PathSegment::Key(k) => cur.get(k).ok_or_else(|| {
                    ToolCachingError::InvalidPath(format!(
                        "path {path}: no `{k}` — {hint}",
                        hint = describe_available(cur)
                    ))
                })?,
                PathSegment::Index(i) => cur.get(*i).ok_or_else(|| {
                    ToolCachingError::InvalidPath(format!(
                        "path {path}: no [{i}] — {hint}",
                        hint = describe_available(cur)
                    ))
                })?,
            };
        }
        Ok(cur)
    }
}

enum PathSegment {
    Key(String),
    Index(usize),
}

pub(crate) fn fetch_text_size_at_path(
    _path: &str,
    value: Value,
) -> Result<usize, ToolCachingError> {
    Ok(fetch_text_for_raw(&value).len())
}

fn fetch_text_for_raw(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Rebuild the structure implied by `path` around `value`. Object-key
/// segments nest into `{key: …}`; array-index segments wrap into a
/// single-element array (`$[3]` → `[value]`) — callers can read
/// `result[0]` and recover the original index from the recovery
/// template's `path` field. Leading `null` padding scales linearly
/// with the index and would burn tokens at every higher position
/// without adding info.
fn wrap_at_path(path: &str, value: Value) -> Result<Value, ToolCachingError> {
    let segments = parse_path(path)?;
    let mut acc = value;
    for seg in segments.into_iter().rev() {
        acc = match seg {
            PathSegment::Key(k) => {
                let mut obj = serde_json::Map::new();
                obj.insert(k, acc);
                Value::Object(obj)
            }
            PathSegment::Index(_) => Value::Array(vec![acc]),
        };
    }
    Ok(acc)
}

/// MCP spec requires `structuredContent` to be a JSON object. When the
/// wrapped value is a scalar or array (string-rooted tools like
/// `k8s_pods_list`, array-rooted upstreams, `$[N]` slices), wrap it in
/// `{"result": <value>}` so MCP clients that validate the envelope as
/// an object don't reject it.
pub fn ensure_object(value: Value) -> Value {
    if matches!(value, Value::Object(_)) {
        value
    } else {
        let mut obj = serde_json::Map::new();
        obj.insert("result".to_string(), value);
        Value::Object(obj)
    }
}

fn parse_path(path: &str) -> Result<Vec<PathSegment>, ToolCachingError> {
    let Some(rest) = path.strip_prefix('$') else {
        return Err(ToolCachingError::InvalidPath(format!(
            "path must start with `$`; got {path}"
        )));
    };
    let bytes = rest.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'.' => {
                i += 1;
                let start = i;
                while i < bytes.len() && bytes[i] != b'.' && bytes[i] != b'[' {
                    i += 1;
                }
                if start == i {
                    return Err(ToolCachingError::InvalidPath(format!(
                        "empty key segment in path {path}"
                    )));
                }
                out.push(PathSegment::Key(rest[start..i].to_string()));
            }
            b'[' => {
                i += 1;
                let start = i;
                while i < bytes.len() && bytes[i] != b']' {
                    i += 1;
                }
                if i == bytes.len() {
                    return Err(ToolCachingError::InvalidPath(format!(
                        "unclosed `[` in path {path}"
                    )));
                }
                let idx: usize = rest[start..i].parse().map_err(|_| {
                    ToolCachingError::InvalidPath(format!("invalid array index in path {path}"))
                })?;
                out.push(PathSegment::Index(idx));
                i += 1;
            }
            _ => {
                return Err(ToolCachingError::InvalidPath(format!(
                    "path after `$` must be `.key` or `[index]`; got {path}"
                )));
            }
        }
    }
    Ok(out)
}

/// Describe what's actually at `cur` so the agent can correct a missing
/// path segment without round-tripping `mode:'summary'`. Lists keys for
/// objects, length for arrays, and the value type otherwise.
fn describe_available(cur: &Value) -> String {
    match cur {
        Value::Object(map) => {
            let mut keys: Vec<&str> = map.keys().map(|s| s.as_str()).collect();
            keys.sort();
            if keys.is_empty() {
                "available keys: (none)".into()
            } else {
                format!("available keys: {keys:?}")
            }
        }
        Value::Array(arr) => format!("array length: {}", arr.len()),
        Value::String(_) => "value is a string (use `query` to slice it)".into(),
        Value::Number(_) | Value::Bool(_) | Value::Null => "value is a scalar".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::{QueryStructure, ToolCallSummary, ToolInvocationId};

    fn stored(root: Value) -> StoredInvocation {
        StoredInvocation {
            id: ToolInvocationId::new(),
            query_structure: QueryStructure { root },
            summary: ToolCallSummary {
                summary: Value::Null,
                wire_result: Value::Null,
                elided_paths: Vec::new(),
                root_path: "$".to_string(),
                total_bytes: 0,
                shown_bytes: 0,
                total_items: None,
                shown_items: None,
                total_lines: None,
                shown_lines: None,
            },
            original_structured: None,
        }
    }

    #[test]
    fn wrap_at_path_root_is_identity() {
        let v = Value::String("hi".into());
        assert_eq!(wrap_at_path("$", v.clone()).unwrap(), v);
    }

    #[test]
    fn ensure_object_wraps_non_objects() {
        assert_eq!(
            ensure_object(Value::String("hi".into())),
            serde_json::json!({"result": "hi"}),
        );
        assert_eq!(
            ensure_object(serde_json::json!(["a", "b"])),
            serde_json::json!({"result": ["a", "b"]}),
        );
        let obj = serde_json::json!({"logs": "x"});
        assert_eq!(ensure_object(obj.clone()), obj);
    }

    #[test]
    fn wrap_at_path_nested_keys() {
        let wrapped = wrap_at_path("$.a.b", Value::String("hi".into())).unwrap();
        assert_eq!(wrapped, serde_json::json!({"a": {"b": "hi"}}));
    }

    #[test]
    fn wrap_at_path_array_root_uses_single_element() {
        let wrapped = wrap_at_path("$[2]", Value::String("hi".into())).unwrap();
        assert_eq!(wrapped, serde_json::json!(["hi"]));
    }

    #[test]
    fn wrap_at_path_mixed_keys_and_indices() {
        let wrapped = wrap_at_path("$.items[1].name", Value::String("hi".into())).unwrap();
        assert_eq!(wrapped, serde_json::json!({"items": [{"name": "hi"}]}),);
    }

    #[test]
    fn parse_path_handles_mixed_keys_and_indices() {
        let segs = parse_path("$.items[3].name").unwrap();
        assert_eq!(segs.len(), 3);
        assert!(matches!(&segs[0], PathSegment::Key(k) if k == "items"));
        assert!(matches!(&segs[1], PathSegment::Index(3)));
        assert!(matches!(&segs[2], PathSegment::Key(k) if k == "name"));
    }

    #[test]
    fn navigate_walks_into_nested_array() {
        let inv = stored(serde_json::json!({"items": ["a", "b", "c"]}));
        assert_eq!(
            inv.navigate("$.items[1]").unwrap(),
            &Value::String("b".into())
        );
    }

    #[test]
    fn fetch_query_range_slices_string() {
        let q = FetchQuery::Range { offset: 3, len: 4 };
        assert_eq!(
            q.apply(Value::String("0123456789".into())).unwrap(),
            Value::String("3456".into()),
        );
    }

    #[test]
    fn fetch_query_range_negative_offset_counts_from_end() {
        let q = FetchQuery::Range { offset: -4, len: 4 };
        assert_eq!(
            q.apply(Value::String("0123456789".into())).unwrap(),
            Value::String("6789".into()),
        );
    }

    #[test]
    fn fetch_query_range_snaps_to_utf8_boundaries() {
        let q = FetchQuery::Range { offset: 1, len: 3 };
        assert_eq!(
            q.apply(Value::String("a▲b".into())).unwrap(),
            Value::String("▲".into()),
        );
    }

    #[test]
    fn fetch_query_range_inside_one_codepoint_returns_empty_string() {
        let q = FetchQuery::Range { offset: 2, len: 1 };
        assert_eq!(
            q.apply(Value::String("a▲b".into())).unwrap(),
            Value::String(String::new()),
        );
    }

    #[test]
    fn fetch_query_lines_negative_offset_returns_tail() {
        let q = FetchQuery::Lines { offset: -2, len: 2 };
        assert_eq!(
            q.apply(Value::String("a\nb\nc\nd\ne".into())).unwrap(),
            Value::String("d\ne".into()),
        );
    }

    #[test]
    fn fetch_query_array_slice_negative_offset_returns_tail() {
        let q = FetchQuery::JsonArraySlice { offset: -2, len: 5 };
        let arr = serde_json::json!([1, 2, 3, 4, 5]);
        assert_eq!(q.apply(arr).unwrap(), serde_json::json!([4, 5]));
    }

    #[test]
    fn fetch_query_negative_offset_larger_than_total_clamps_to_zero() {
        let q = FetchQuery::Lines {
            offset: -999,
            len: 3,
        };
        assert_eq!(
            q.apply(Value::String("a\nb\nc\nd".into())).unwrap(),
            Value::String("a\nb\nc".into()),
        );
    }

    #[test]
    fn fetch_query_lines_accepts_negative_offset_via_json() {
        let q: FetchQuery = serde_json::from_value(serde_json::json!({
            "mode": "lines", "offset": -3, "len": 3
        }))
        .expect("negative offset deserializes");
        assert!(matches!(q, FetchQuery::Lines { offset: -3, len: 3 }));
    }

    #[test]
    fn navigate_missing_key_error_lists_available_keys() {
        let inv = stored(serde_json::json!({"logs": "x", "extra": 1}));
        let err = inv.navigate("$.result").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no `result`"), "got: {msg}");
        assert!(msg.contains("logs"), "got: {msg}");
        assert!(msg.contains("extra"), "got: {msg}");
    }

    #[test]
    fn navigate_missing_index_error_lists_array_length() {
        let inv = stored(serde_json::json!([1, 2, 3]));
        let err = inv.navigate("$[7]").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no [7]"), "got: {msg}");
        assert!(msg.contains("array length: 3"), "got: {msg}");
    }

    #[test]
    fn navigate_missing_key_on_scalar_says_scalar() {
        let inv = stored(serde_json::json!("hello"));
        let err = inv.navigate("$.foo").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("value is a string"), "got: {msg}");
    }

    #[test]
    fn too_large_hint_always_suggests_summary_mode() {
        let inv = stored(serde_json::json!({"result": "x"}));
        let hint = inv.too_large_hint("$.result", 64);
        assert!(hint.contains("mode: \"summary\""), "got: {hint}");
        assert!(hint.contains("summary_envelope_bytes:"), "got: {hint}");
        assert!(hint.contains("normal_fetch_limit_bytes: 64"), "got: {hint}");
    }

    #[test]
    fn summary_query_bypasses_fetch_cap_without_late_annotation() {
        let mut inv = stored(serde_json::json!("full"));
        inv.summary = ToolCallSummary {
            summary: Value::String("x".repeat(200)),
            wire_result: Value::String("<summary>".into()),
            elided_paths: Vec::new(),
            root_path: "$".to_string(),
            total_bytes: 200,
            shown_bytes: 200,
            total_items: None,
            shown_items: None,
            total_lines: None,
            shown_lines: None,
        };

        let fetched = inv
            .query("$", Some(&FetchQuery::Summary), 64)
            .expect("summary mode bypasses the normal fetch cap");
        let text = match fetched.result.content.first().map(|c| &c.raw) {
            Some(rmcp::model::RawContent::Text(t)) => t.text.as_str(),
            _ => "",
        };
        assert!(!text.contains("<summary-fetch"), "got: {text}");
        assert!(fetched.structured.get("_fetch").is_none());
        assert_eq!(
            fetched.result.structured_content.as_ref(),
            Some(&fetched.structured),
            "FetchResult result.structured_content must stay in sync with structured"
        );
    }

    #[test]
    fn too_large_hint_lists_sub_invocations_when_root_has_them() {
        let inv = stored(serde_json::json!({
            "result": "x",
            "sub_invocations": [
                {"invocation_id": "aaa", "tool_name": "github_list_prs"},
                {"invocation_id": "bbb", "tool_name": "honeycomb_query"},
            ],
        }));
        let hint = inv.too_large_hint("$.result", 1024);
        assert!(hint.contains("github_list_prs=aaa"), "got: {hint}");
        assert!(hint.contains("honeycomb_query=bbb"), "got: {hint}");
    }

    #[test]
    fn too_large_hint_caps_to_three_and_mentions_overflow() {
        let inv = stored(serde_json::json!({
            "result": "x",
            "sub_invocations": [
                {"invocation_id": "i0", "tool_name": "t0"},
                {"invocation_id": "i1", "tool_name": "t1"},
                {"invocation_id": "i2", "tool_name": "t2"},
                {"invocation_id": "i3", "tool_name": "t3"},
                {"invocation_id": "i4", "tool_name": "t4"},
            ],
        }));
        let hint = inv.too_large_hint("$.result", 1024);
        assert!(hint.contains("t0=i0"), "got: {hint}");
        assert!(hint.contains("t2=i2"), "got: {hint}");
        assert!(!hint.contains("t3=i3"), "should cap at 3; got: {hint}");
        assert!(hint.contains("+2 more"), "got: {hint}");
    }

    #[test]
    fn too_large_hint_skips_sub_invocations_when_path_is_unrelated() {
        let inv = stored(serde_json::json!({
            "result": "x",
            "sub_invocations": [
                {"invocation_id": "aaa", "tool_name": "github_list_prs"},
            ],
        }));
        let hint = inv.too_large_hint("$.sub_invocations", 1024);
        assert!(!hint.contains("github_list_prs"), "got: {hint}");
        assert!(hint.contains("mode: \"summary\""), "got: {hint}");
    }
}
