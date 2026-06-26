//! Recovery side of tool-caching: given a stored invocation, navigate
//! a json-path and optionally slice the resolved value.

use rmcp::model::{CallToolResult, Content};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ToolCachingError;
use crate::repo::StoredInvocation;

/// Liberal numeric deserialization — some models send `"200"` instead of
/// `200`; coerce numeric strings rather than failing the whole fetch.
mod liberal {
    use serde::Deserialize;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrInt {
        Int(i64),
        Str(String),
    }

    pub(super) fn deserialize_i64<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<i64, D::Error> {
        match StringOrInt::deserialize(deserializer)? {
            StringOrInt::Int(v) => Ok(v),
            StringOrInt::Str(s) => s.trim().parse().map_err(serde::de::Error::custom),
        }
    }

    pub(super) fn deserialize_usize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<usize, D::Error> {
        match StringOrInt::deserialize(deserializer)? {
            StringOrInt::Int(v) => usize::try_from(v).map_err(serde::de::Error::custom),
            StringOrInt::Str(s) => s.trim().parse().map_err(serde::de::Error::custom),
        }
    }
}

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
    Range {
        #[serde(deserialize_with = "liberal::deserialize_i64")]
        offset: i64,
        #[serde(deserialize_with = "liberal::deserialize_usize")]
        len: usize,
    },
    /// Line range on a string-typed value at `path`. `offset` is the
    /// zero-indexed first line to return; `len` is the line count.
    /// `offset` may be negative to count from the end (e.g.
    /// `offset:-80, len:80` returns the last 80 lines).
    Lines {
        #[serde(deserialize_with = "liberal::deserialize_i64")]
        offset: i64,
        #[serde(deserialize_with = "liberal::deserialize_usize")]
        len: usize,
    },
    /// Item range on an array-typed value at `path`. Returns the slice
    /// `arr[offset..offset+len]` as a `Value::Array`. `offset` may be
    /// negative to count from the end of the array.
    JsonArraySlice {
        #[serde(deserialize_with = "liberal::deserialize_i64")]
        offset: i64,
        #[serde(deserialize_with = "liberal::deserialize_usize")]
        len: usize,
    },
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

pub(crate) struct ShrunkSlice {
    pub value: Value,
    pub original_len: usize,
    pub actual_len: usize,
    pub unit: &'static str,
    /// The next offset to use for paging through the remainder.
    pub next_offset: usize,
}

impl FetchQuery {
    /// Try to produce a smaller slice that fits within `max_bytes`.
    /// Returns `None` if shrinking isn't applicable to this query mode.
    fn shrink_to_fit(
        &self,
        resolved: Value,
        max_bytes: usize,
    ) -> Result<Option<ShrunkSlice>, ToolCachingError> {
        match self {
            FetchQuery::Lines { offset, len } => {
                let s = resolved.as_str().ok_or_else(|| {
                    ToolCachingError::InvalidPath(
                        "lines query requires a string at the resolved path".into(),
                    )
                })?;
                let all: Vec<&str> = s.lines().collect();
                let start = resolve_offset(*offset, all.len());
                let end = start.saturating_add(*len).min(all.len());
                let original_len = end - start;
                let mut hi = end;
                while hi > start {
                    let candidate = all[start..hi].join("\n");
                    if candidate.len() <= max_bytes {
                        return Ok(Some(ShrunkSlice {
                            value: Value::String(candidate),
                            original_len,
                            actual_len: hi - start,
                            unit: "lines",
                            next_offset: hi,
                        }));
                    }
                    let ratio = max_bytes as f64 / candidate.len() as f64;
                    let new_count = ((hi - start) as f64 * ratio * 0.9) as usize;
                    let next_hi = start + new_count.max(1);
                    if next_hi >= hi {
                        hi -= 1;
                    } else {
                        hi = next_hi;
                    }
                }
                Ok(None)
            }
            FetchQuery::JsonArraySlice { offset, len } => {
                let arr = resolved.as_array().ok_or_else(|| {
                    ToolCachingError::InvalidPath(
                        "json_array_slice query requires an array at the resolved path".into(),
                    )
                })?;
                let start = resolve_offset(*offset, arr.len());
                let end = start.saturating_add(*len).min(arr.len());
                let original_len = end - start;
                let mut hi = end;
                while hi > start {
                    let slice = Value::Array(arr[start..hi].to_vec());
                    let text = fetch_text_for_raw(&slice);
                    if text.len() <= max_bytes {
                        return Ok(Some(ShrunkSlice {
                            value: slice,
                            original_len,
                            actual_len: hi - start,
                            unit: "items",
                            next_offset: hi,
                        }));
                    }
                    let ratio = max_bytes as f64 / text.len() as f64;
                    let new_count = ((hi - start) as f64 * ratio * 0.9) as usize;
                    let next_hi = start + new_count.max(1);
                    if next_hi >= hi {
                        hi -= 1;
                    } else {
                        hi = next_hi;
                    }
                }
                Ok(None)
            }
            FetchQuery::Range { offset, len } => {
                let s = resolved.as_str().ok_or_else(|| {
                    ToolCachingError::InvalidPath(
                        "range query requires a string at the resolved path".into(),
                    )
                })?;
                let (start, _end) = resolve_utf8_byte_window(s, *offset, *len);
                let capped_len = max_bytes.min(s.len() - start);
                let (actual_start, actual_end) =
                    resolve_utf8_byte_window_usize(s, start, capped_len);
                let slice = &s[actual_start..actual_end];
                Ok(Some(ShrunkSlice {
                    value: Value::String(slice.to_string()),
                    original_len: *len,
                    actual_len: actual_end - actual_start,
                    unit: "bytes",
                    next_offset: actual_end,
                }))
            }
            FetchQuery::Summary => Ok(None),
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
    /// the caller's request (`$.foo[2]` → `{"foo": [X]}`). When the
    /// sliced result exceeds `max_bytes`, the window is automatically
    /// shrunk to fit rather than returning an error.
    pub fn query(
        &self,
        path: &str,
        query: Option<&FetchQuery>,
        max_bytes: usize,
    ) -> Result<FetchResult, ToolCachingError> {
        if matches!(query, Some(FetchQuery::Summary)) {
            return self.summary_view(max_bytes);
        }
        let resolved = self.navigate(path)?.clone();
        let sliced = match query {
            Some(q) => q.apply(resolved.clone())?,
            None => resolved.clone(),
        };
        let text = fetch_text_for_raw(&sliced);

        if text.len() <= max_bytes {
            let wrapped = wrap_at_path(path, sliced)?;
            return Ok(FetchResult {
                result: CallToolResult::success(vec![Content::text(text)]),
                structured: wrapped,
            });
        }

        let mut shrink_failed = false;
        if let Some(q) = query {
            match q.shrink_to_fit(resolved, max_bytes)? {
                Some(shrunk) => {
                    let text = fetch_text_for_raw(&shrunk.value);
                    let wrapped = wrap_at_path(path, shrunk.value)?;
                    let remaining = shrunk.original_len - shrunk.actual_len;
                    let note = format!(
                        "\n\n[TRUNCATED: returned {actual} of {requested} {unit} \
                         ({remaining} remaining). Fetch the next page with \
                         offset: {next_offset}]",
                        actual = shrunk.actual_len,
                        requested = shrunk.original_len,
                        unit = shrunk.unit,
                        remaining = remaining,
                        next_offset = shrunk.next_offset,
                    );
                    return Ok(FetchResult {
                        result: CallToolResult::success(vec![Content::text(format!(
                            "{text}{note}"
                        ))]),
                        structured: wrapped,
                    });
                }
                None => {
                    // Shrink attempted but even the smallest slice exceeds the cap —
                    // typically a single array element with elided sub-fields.
                    shrink_failed = matches!(
                        q,
                        FetchQuery::JsonArraySlice { .. }
                            | FetchQuery::Lines { .. }
                            | FetchQuery::Range { .. }
                    );
                }
            }
        }

        Err(ToolCachingError::FetchResponseTooLarge {
            size: text.len(),
            max: max_bytes,
            hint: self.too_large_hint(path, max_bytes, shrink_failed),
        })
    }

    /// Replay the curated `<summary>+<recovery>` envelope from the
    /// persisted `ToolCallSummary`. Summary mode is the recovery escape
    /// hatch, so it bypasses the normal fetch-size cap.
    ///
    /// Envelope-mode invocations (e.g. `compose`, which advertises
    /// `ComposeOutput` as its outputSchema and has no outer `{result: …}`
    /// wrapper) replay through `build_wire_envelope` so the replayed
    /// shape matches what the live call emitted — agents using
    /// `query:{mode:"summary"}` recovery see the same structured shape
    /// they would have seen calling the tool directly.
    fn summary_view(&self, _max_bytes: usize) -> Result<FetchResult, ToolCachingError> {
        let (result, structured) = if self.summary.envelope_mode {
            self.summary.build_wire_envelope(self.id)
        } else {
            self.summary.build_wire(self.id)
        };
        Ok(FetchResult { result, structured })
    }

    /// Tail-appended hint for `FetchResponseTooLarge`. Surfaces, in order:
    /// (1) when the upstream walker advertised elided sub-paths under the
    /// requested path, lists up to three with their byte sizes and the
    /// exact `tool_output_fetch` template the agent should copy — this is
    /// the case where even the smallest array slice can't fit because one
    /// element carries a multi-KB elided field; (2) `mode:'summary'` as
    /// the always-available escape hatch; (3) on a compose root, names
    /// drill-down sub-invocation ids.
    fn too_large_hint(&self, path: &str, max_bytes: usize, shrink_failed: bool) -> String {
        let summary_envelope_bytes = self.summary.build_envelope_text().len();
        let mut parts = Vec::new();

        if let Some(advertised) = self.advertised_paths_hint(path, shrink_failed) {
            parts.push(advertised);
        }

        parts.push(format!(
            ". Try `query: {{mode: \"summary\"}}` for the curated envelope \
             (summary_envelope_bytes: {summary_envelope_bytes}, \
             normal_fetch_limit_bytes: {max_bytes}; summary mode bypasses that cap)"
        ));

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

    /// Find persisted `elided_paths` that fall under the requested fetch
    /// `path` and render the first few as a copy-pasteable hint. Returns
    /// `None` when nothing useful is under this path.
    fn advertised_paths_hint(&self, path: &str, shrink_failed: bool) -> Option<String> {
        let matching_count = self
            .summary
            .elided_paths
            .iter()
            .filter(|ep| path_is_ancestor_or_equal(path, &ep.path))
            .count();
        let nested: Vec<&crate::primitives::ElidedPath> = self
            .summary
            .elided_paths
            .iter()
            .filter(|ep| path_is_ancestor_or_equal(path, &ep.path))
            .take(4)
            .collect();
        if nested.is_empty() {
            return None;
        }
        let lead = if shrink_failed {
            ". This invocation has per-item elisions; even the smallest \
             slice exceeds the cap because individual elements carry \
             multi-KB elided fields. Fetch those fields directly using \
             the advertised paths"
        } else {
            ". The upstream walker advertised elided sub-paths under \
             this path; fetch them directly instead of slicing the parent"
        };
        let rendered: Vec<String> = nested
            .iter()
            .map(|ep| {
                let dims = elided_dims_summary(ep);
                let template = serde_json::to_string(&ep.recover).unwrap_or_default();
                format!("\n  - {} ({}) — {}", ep.path, dims, template)
            })
            .collect();
        let extra = matching_count.saturating_sub(nested.len());
        let trailer = if extra > 0 {
            format!("\n  - (+{extra} more in `_recovery.paths`)")
        } else {
            String::new()
        };
        Some(format!(
            "{lead}:{rendered}{trailer}",
            rendered = rendered.join("")
        ))
    }

    fn navigate(&self, path: &str) -> Result<&Value, ToolCachingError> {
        let segments = parse_path(path)?;
        let root = &self.query_structure.root;
        match walk_segments(root, &segments, path) {
            Ok(v) => Ok(v),
            // Tolerate the MCP wire-wrapper assumption: models routinely send
            // `$.result.<...>` because normal tool responses are wrapped in
            // `{result: ...}` on the wire, but the persisted fetch root is the
            // unwrapped upstream value. When the root has no `result` key and
            // the `$.result`-stripped path resolves, honour that rather than
            // failing — but only when stripping actually resolves, so genuine
            // misses still surface the original key-listing error.
            Err(original) => match segments.as_slice() {
                [PathSegment::Key(first), rest @ ..]
                    if first == "result" && !rest.is_empty() && root.get("result").is_none() =>
                {
                    match walk_segments(root, rest, path) {
                        Ok(v) => {
                            tracing::debug!(
                                path = %path,
                                "drua_core.tool_caching.fetch_stripped_result_wrapper"
                            );
                            Ok(v)
                        }
                        Err(_) => Err(original),
                    }
                }
                _ => Err(original),
            },
        }
    }
}

fn walk_segments<'a>(
    root: &'a Value,
    segments: &[PathSegment],
    path: &str,
) -> Result<&'a Value, ToolCachingError> {
    let mut cur = root;
    for seg in segments {
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

#[derive(Debug, PartialEq)]
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

pub fn fetch_text_for_raw(value: &Value) -> String {
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
                if i < bytes.len() && bytes[i] == b'"' {
                    i += 1;
                    let mut key = String::new();
                    let mut span_start = i;
                    while i < bytes.len() {
                        if bytes[i] == b'\\' && i + 1 < bytes.len() {
                            key.push_str(&rest[span_start..i]);
                            let escaped_char_len =
                                rest[i + 1..].chars().next().map_or(1, |c| c.len_utf8());
                            match bytes[i + 1] {
                                b'"' => key.push('"'),
                                b'\\' => key.push('\\'),
                                _ => key.push_str(&rest[i..i + 1 + escaped_char_len]),
                            }
                            i += 1 + escaped_char_len;
                            span_start = i;
                        } else if bytes[i] == b'"' {
                            key.push_str(&rest[span_start..i]);
                            break;
                        } else {
                            i += 1;
                        }
                    }
                    if i == bytes.len() {
                        return Err(ToolCachingError::InvalidPath(format!(
                            "unclosed `[\"` in path {path}"
                        )));
                    }
                    i += 1; // skip closing "
                    if i >= bytes.len() || bytes[i] != b']' {
                        return Err(ToolCachingError::InvalidPath(format!(
                            "expected `]` after quoted key in path {path}"
                        )));
                    }
                    i += 1; // skip ]
                    out.push(PathSegment::Key(key));
                } else {
                    // Numeric array index: [0]
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

/// True when `ancestor` is the root (`$`), equals `descendant`, or is a
/// strict path-prefix of `descendant` at a segment boundary. Avoids
/// false positives like `$.items` ancestor-of `$.itemsX`.
fn path_is_ancestor_or_equal(ancestor: &str, descendant: &str) -> bool {
    if ancestor == "$" {
        return descendant == "$" || descendant.starts_with("$.") || descendant.starts_with("$[");
    }
    if ancestor == descendant {
        return true;
    }
    if let Some(rest) = descendant.strip_prefix(ancestor) {
        return rest.starts_with('.') || rest.starts_with('[');
    }
    false
}

/// One-line "43337 bytes, 524 lines" summary of an ElidedPath's dimensions.
fn elided_dims_summary(ep: &crate::primitives::ElidedPath) -> String {
    let mut parts = vec![format!("{} bytes", ep.total_bytes)];
    if let Some(n) = ep.total_lines {
        parts.push(format!("{n} lines"));
    }
    if let Some(n) = ep.total_items {
        parts.push(format!("{n} items"));
    }
    parts.join(", ")
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
                envelope_mode: false,
            },
            original_structured: None,
        }
    }

    #[test]
    fn fetch_query_coerces_numeric_strings() {
        let q: FetchQuery = serde_json::from_value(
            serde_json::json!({"mode": "lines", "offset": "-80", "len": "200"}),
        )
        .unwrap();
        assert!(matches!(
            q,
            FetchQuery::Lines {
                offset: -80,
                len: 200
            }
        ));
        let q: FetchQuery =
            serde_json::from_value(serde_json::json!({"mode": "range", "offset": "0", "len": 64}))
                .unwrap();
        assert!(matches!(q, FetchQuery::Range { offset: 0, len: 64 }));
        let q: FetchQuery = serde_json::from_value(
            serde_json::json!({"mode": "json_array_slice", "offset": 5, "len": "10"}),
        )
        .unwrap();
        assert!(matches!(
            q,
            FetchQuery::JsonArraySlice { offset: 5, len: 10 }
        ));
        assert!(serde_json::from_value::<FetchQuery>(
            serde_json::json!({"mode": "lines", "offset": 0, "len": "abc"})
        )
        .is_err());
        assert!(serde_json::from_value::<FetchQuery>(
            serde_json::json!({"mode": "lines", "offset": 0, "len": "-5"})
        )
        .is_err());
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
    fn parse_path_bracket_quoted_key_with_dot() {
        let segs = parse_path(r#"$.files["values.yaml"].content"#).unwrap();
        assert_eq!(segs.len(), 3);
        assert!(matches!(&segs[0], PathSegment::Key(k) if k == "files"));
        assert!(matches!(&segs[1], PathSegment::Key(k) if k == "values.yaml"));
        assert!(matches!(&segs[2], PathSegment::Key(k) if k == "content"));
    }

    #[test]
    fn parse_path_bracket_quoted_key_with_escaped_quote() {
        let segs = parse_path(r#"$.files["he said \"hi\".txt"]"#).unwrap();
        assert_eq!(segs.len(), 2);
        assert!(matches!(&segs[0], PathSegment::Key(k) if k == "files"));
        assert!(matches!(&segs[1], PathSegment::Key(k) if k == r#"he said "hi".txt"#));
    }

    #[test]
    fn navigate_bracket_quoted_key_resolves_dotted_key() {
        let inv = stored(serde_json::json!({"files": {"values.yaml": {"content": "x"}}}));
        assert_eq!(
            inv.navigate(r#"$.files["values.yaml"].content"#).unwrap(),
            &Value::String("x".into())
        );
    }

    #[test]
    fn wrap_at_path_bracket_quoted_key() {
        let wrapped =
            wrap_at_path(r#"$.files["values.yaml"]"#, Value::String("hi".into())).unwrap();
        assert_eq!(wrapped, serde_json::json!({"files": {"values.yaml": "hi"}}));
    }

    #[test]
    fn parse_path_bracket_quoted_key_with_unicode() {
        let segs = parse_path(r#"$.files["données.txt"]"#).unwrap();
        assert_eq!(
            segs,
            vec![
                PathSegment::Key("files".into()),
                PathSegment::Key("données.txt".into()),
            ]
        );
    }

    #[test]
    fn parse_path_bracket_backslash_before_multibyte_no_panic() {
        let segs = parse_path(r#"$.x["\é"]"#).unwrap();
        assert_eq!(
            segs,
            vec![PathSegment::Key("x".into()), PathSegment::Key("\\é".into()),]
        );
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
    fn navigate_tolerates_result_wrapper_prefix() {
        // Models send `$.result.<...>` assuming the MCP wire wrapper; the
        // persisted root is unwrapped. Strip the bogus `result` segment when
        // it lets the rest of the path resolve.
        let inv = stored(serde_json::json!({"issues": [{"body": "boom"}]}));
        assert_eq!(
            inv.navigate("$.result.issues[0].body").unwrap(),
            &Value::String("boom".into())
        );
    }

    #[test]
    fn navigate_tolerates_result_wrapper_for_content_shape() {
        let inv = stored(serde_json::json!({"content": [{"text": "hi"}]}));
        assert_eq!(
            inv.navigate("$.result.content[0].text").unwrap(),
            &Value::String("hi".into())
        );
    }

    #[test]
    fn navigate_keeps_real_result_key() {
        // A genuine `result` key must still resolve normally, not be stripped.
        let inv = stored(serde_json::json!({"result": {"body": "real"}}));
        assert_eq!(
            inv.navigate("$.result.body").unwrap(),
            &Value::String("real".into())
        );
    }

    #[test]
    fn navigate_result_strip_does_not_mask_real_miss() {
        // Stripping `result` doesn't resolve either → surface the original
        // error against the path the caller actually sent.
        let inv = stored(serde_json::json!({"logs": "x"}));
        let err = inv.navigate("$.result.missing").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no `result`"), "got: {msg}");
        assert!(msg.contains("logs"), "got: {msg}");
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
        let hint = inv.too_large_hint("$.result", 64, false);
        assert!(hint.contains("mode: \"summary\""), "got: {hint}");
        assert!(hint.contains("summary_envelope_bytes:"), "got: {hint}");
        assert!(hint.contains("normal_fetch_limit_bytes: 64"), "got: {hint}");
    }

    /// Envelope-mode persisted summaries (e.g. `compose`) must replay
    /// through `build_wire_envelope` on `query:{mode:"summary"}` so the
    /// re-fetched structured shape matches the live elided call. If it
    /// fell back to `build_wire`, agents would see `{result: <T>, ...}`
    /// from the replay but a flat ComposeOutput from the live call.
    #[test]
    fn summary_query_replays_envelope_mode_through_build_wire_envelope() {
        use crate::primitives::ElidedPath;
        let mut inv = stored(serde_json::json!({}));
        inv.summary = ToolCallSummary {
            summary: Value::String("<elided body>".into()),
            wire_result: serde_json::json!({
                "sub_invocations": [],
                "tool_calls": 1,
                "execution_time_ms": 12,
                "console": [],
                "result": "<elided body>",
            }),
            elided_paths: vec![ElidedPath {
                path: "$.result".to_string(),
                total_bytes: 9999,
                shown_bytes: 16,
                total_lines: None,
                shown_lines: None,
                total_items: None,
                shown_items: None,
                recover: serde_json::json!({
                    "tool": "tool_output_fetch",
                    "args_template": { "invocation_id": "<id>", "path": "$.result" },
                }),
            }],
            root_path: "$".to_string(),
            total_bytes: 9999,
            shown_bytes: 16,
            total_items: None,
            shown_items: None,
            total_lines: None,
            shown_lines: None,
            envelope_mode: true,
        };

        let fetched = inv
            .query("$", Some(&FetchQuery::Summary), 64)
            .expect("summary mode bypasses the normal fetch cap");
        // ComposeOutput fields at the root, not under a synthetic `result` key.
        assert_eq!(fetched.structured["tool_calls"], 1);
        assert_eq!(fetched.structured["execution_time_ms"], 12);
        assert_eq!(fetched.structured["result"], "<elided body>");
        assert!(fetched.structured.get("_recovery").is_some());
        assert!(fetched.structured.get("_elided").is_some());
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
            envelope_mode: false,
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
        let hint = inv.too_large_hint("$.result", 1024, false);
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
        let hint = inv.too_large_hint("$.result", 1024, false);
        assert!(hint.contains("t0=i0"), "got: {hint}");
        assert!(hint.contains("t2=i2"), "got: {hint}");
        assert!(!hint.contains("t3=i3"), "should cap at 3; got: {hint}");
        assert!(hint.contains("+2 more"), "got: {hint}");
    }

    fn stored_with_elided(
        root: Value,
        elided_paths: Vec<crate::primitives::ElidedPath>,
    ) -> StoredInvocation {
        StoredInvocation {
            id: ToolInvocationId::new(),
            query_structure: QueryStructure { root },
            summary: ToolCallSummary {
                summary: Value::Null,
                wire_result: Value::Null,
                elided_paths,
                root_path: "$".to_string(),
                total_bytes: 0,
                shown_bytes: 0,
                total_items: None,
                shown_items: None,
                total_lines: None,
                shown_lines: None,
                envelope_mode: false,
            },
            original_structured: None,
        }
    }

    fn elided(
        path: &str,
        total_bytes: u64,
        total_lines: Option<u32>,
    ) -> crate::primitives::ElidedPath {
        crate::primitives::ElidedPath {
            path: path.to_string(),
            total_bytes,
            shown_bytes: 0,
            total_lines,
            shown_lines: total_lines.map(|_| 1),
            total_items: None,
            shown_items: None,
            recover: serde_json::json!({
                "tool": "tool_output_fetch",
                "args_template": {"invocation_id": "x", "path": path},
            }),
        }
    }

    #[test]
    fn path_ancestor_check_handles_root_keys_and_indices() {
        assert!(path_is_ancestor_or_equal("$", "$.items[0].body"));
        assert!(path_is_ancestor_or_equal("$", "$[3]"));
        assert!(path_is_ancestor_or_equal("$", "$"));
        assert!(path_is_ancestor_or_equal("$.items", "$.items[0].body"));
        assert!(path_is_ancestor_or_equal("$.items", "$.items"));
        // segment-boundary check: $.items must not match $.itemsX
        assert!(!path_is_ancestor_or_equal("$.items", "$.itemsX"));
        assert!(!path_is_ancestor_or_equal("$.items", "$.other"));
    }

    #[test]
    fn too_large_hint_surfaces_advertised_paths_under_requested_path() {
        let inv = stored_with_elided(
            serde_json::json!({"items": [{"body": "x"}]}),
            vec![
                elided("$.items[0].body", 43337, Some(524)),
                elided("$.items[1].body", 1521, Some(23)),
            ],
        );
        let hint = inv.too_large_hint("$.items", 16384, true);
        assert!(hint.contains("$.items[0].body"), "got: {hint}");
        assert!(hint.contains("43337 bytes"), "got: {hint}");
        assert!(hint.contains("524 lines"), "got: {hint}");
        assert!(hint.contains("$.items[1].body"), "got: {hint}");
        assert!(
            hint.contains("smallest slice exceeds the cap"),
            "shrink_failed lead should appear: {hint}"
        );
    }

    #[test]
    fn too_large_hint_uses_softer_lead_when_shrink_was_not_attempted() {
        let inv = stored_with_elided(
            serde_json::json!({"logs": "x"}),
            vec![elided("$.logs", 50000, Some(900))],
        );
        let hint = inv.too_large_hint("$", 16384, false);
        assert!(hint.contains("advertised elided sub-paths"), "got: {hint}");
        assert!(!hint.contains("smallest slice"), "got: {hint}");
    }

    #[test]
    fn too_large_hint_caps_advertised_paths_to_four_with_overflow_note() {
        let inv = stored_with_elided(
            serde_json::json!({"items": []}),
            vec![
                elided("$.items[0].body", 1000, None),
                elided("$.items[1].body", 1000, None),
                elided("$.items[2].body", 1000, None),
                elided("$.items[3].body", 1000, None),
                elided("$.items[4].body", 1000, None),
                elided("$.items[5].body", 1000, None),
            ],
        );
        let hint = inv.too_large_hint("$.items", 16384, true);
        assert!(hint.contains("$.items[3].body"), "got: {hint}");
        assert!(!hint.contains("$.items[4].body"), "should cap at 4: {hint}");
        assert!(hint.contains("+2 more in `_recovery.paths`"), "got: {hint}");
    }

    #[test]
    fn too_large_hint_skips_advertised_paths_outside_requested_path() {
        let inv = stored_with_elided(
            serde_json::json!({"items": [], "other": "x"}),
            vec![elided("$.other.body", 50000, None)],
        );
        let hint = inv.too_large_hint("$.items", 16384, true);
        assert!(!hint.contains("$.other.body"), "got: {hint}");
        // still falls back to the summary suggestion
        assert!(hint.contains("mode: \"summary\""), "got: {hint}");
    }

    #[test]
    fn too_large_hint_skips_sub_invocations_when_path_is_unrelated() {
        let inv = stored(serde_json::json!({
            "result": "x",
            "sub_invocations": [
                {"invocation_id": "aaa", "tool_name": "github_list_prs"},
            ],
        }));
        let hint = inv.too_large_hint("$.sub_invocations", 1024, false);
        assert!(!hint.contains("github_list_prs"), "got: {hint}");
        assert!(hint.contains("mode: \"summary\""), "got: {hint}");
    }
}
