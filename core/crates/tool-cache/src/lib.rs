//! Persistent storage for classified tool-invocation outputs.

mod entity;
mod error;
mod grep;
mod repo;

pub use entity::{
    InvocationOwner, NewToolInvocation, ToolInvocation, ToolInvocationId, ToolInvocationOwnerId,
};
pub use error::ToolInvocationError;

use rmcp::model::{CallToolResult, Content};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::instrument;

use drua_tool_classifier::{Classification, ToolResultSummary, RECOVERY_INVOCATION_PLACEHOLDER};
use repo::ToolInvocationRepo;

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum FetchQuery {
    Tail {
        lines: u32,
    },
    Head {
        lines: u32,
    },
    Range {
        offset: u64,
        len: u32,
    },
    /// JSON-path lookup against the persisted `original_structured`.
    /// `path` like `$.foo.bar[3]`; returns the value at that path.
    JsonPath {
        path: String,
    },
    /// JSON-path lookup that resolves to an array, then slices it.
    /// `path` like `$.hits`; returns `array[offset..offset+len]`.
    JsonArraySlice {
        path: String,
        offset: u32,
        len: u32,
    },
    /// Line-grep with rg-style flags; cross-line patterns not supported.
    Grep {
        pattern: String,

        #[serde(rename = "-i", default)]
        case_insensitive: bool,

        #[serde(rename = "-A", default, skip_serializing_if = "Option::is_none")]
        after_context: Option<u32>,

        #[serde(rename = "-B", default, skip_serializing_if = "Option::is_none")]
        before_context: Option<u32>,

        /// Symmetric -C; ignored when -A or -B is set.
        #[serde(rename = "-C", default, skip_serializing_if = "Option::is_none")]
        context: Option<u32>,

        #[serde(rename = "-n", default = "default_true")]
        line_numbers: bool,

        #[serde(default)]
        invert_match: bool,

        #[serde(default, skip_serializing_if = "Option::is_none")]
        head_limit: Option<u32>,
    },
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct FetchResult {
    pub content: String,
    pub total_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elision: Option<FetchElision>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct FetchElision {
    pub slice_bytes: u64,
    pub kept_bytes: u64,
    pub hint: String,
}

const MAX_GREP_PATTERN_LENGTH: usize = 1000;

const MAX_FETCH_RESPONSE_BYTES: usize = 32 * 1024;

#[derive(Clone)]
pub struct ToolInvocations {
    repo: ToolInvocationRepo,
}

impl ToolInvocations {
    pub fn new(pool: &sqlx::PgPool) -> Self {
        Self {
            repo: ToolInvocationRepo::new(pool),
        }
    }

    #[instrument(name = "tool_cache.persist", skip_all)]
    pub async fn persist(
        &self,
        new: NewToolInvocation,
    ) -> Result<ToolInvocation, ToolInvocationError> {
        Ok(self.repo.create(new).await?)
    }

    #[instrument(name = "tool_cache.find_by_id", skip(self))]
    pub async fn find_by_id(
        &self,
        id: ToolInvocationId,
    ) -> Result<ToolInvocation, ToolInvocationError> {
        Ok(self.repo.find_by_id(id).await?)
    }

    #[instrument(name = "tool_cache.fetch", skip(self))]
    pub async fn fetch(
        &self,
        id: ToolInvocationId,
        query: FetchQuery,
    ) -> Result<FetchResult, ToolInvocationError> {
        let invocation = self.repo.find_by_id(id).await?;
        apply_fetch_query(
            &invocation.raw_text,
            invocation.original_structured.as_ref(),
            &query,
        )
    }

    #[instrument(name = "tool_cache.find_for_diff", skip(self))]
    pub async fn find_for_diff(
        &self,
        owner: InvocationOwner,
        args_hash: &[u8],
    ) -> Result<Option<ToolInvocation>, ToolInvocationError> {
        Ok(self.repo.find_latest_by_args_hash(owner, args_hash).await?)
    }

    #[instrument(name = "tool_cache.persist_classification", skip_all)]
    #[allow(clippy::too_many_arguments)]
    pub async fn persist_classification(
        &self,
        owner: impl Into<InvocationOwner>,
        tool_name: &str,
        args: &serde_json::Value,
        classification: Classification,
        original_structured: Option<serde_json::Value>,
        duration_ms: u64,
        started_at: chrono::DateTime<chrono::Utc>,
    ) -> Option<PersistedClassification> {
        let owner = owner.into();
        let Classification {
            summary,
            canonical_text,
        } = classification;
        let raw_size_bytes = canonical_text.len() as i64;

        let summary_value = match serde_json::to_value(&summary) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "failed to serialise summary; skipping persistence");
                return None;
            }
        };

        let canonical_args = match serde_json::to_string(args) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "failed to canonicalise args; skipping persistence");
                return None;
            }
        };
        let mut hasher = Sha256::new();
        hasher.update(canonical_args.as_bytes());
        let args_hash = hasher.finalize().to_vec();

        let new = NewToolInvocation {
            owner,
            tool_name: tool_name.to_string(),
            args: args.clone(),
            args_hash,
            classifier: summary.kind().to_string(),
            summary: summary_value.clone(),
            raw_text: canonical_text,
            raw_size_bytes,
            original_structured,
            exit_code: None,
            duration_ms: duration_ms.min(i32::MAX as u64) as i32,
            started_at,
        };

        let persisted = match self.persist(new).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "failed to persist tool invocation; falling back to raw result"
                );
                return None;
            }
        };

        Some(PersistedClassification {
            invocation_id: persisted.id,
            summary,
            summary_value,
            raw_size_bytes: raw_size_bytes as u64,
        })
    }

    #[instrument(name = "tool_cache.persist_and_envelope", skip_all)]
    #[allow(clippy::too_many_arguments)]
    pub async fn persist_and_envelope(
        &self,
        owner: impl Into<InvocationOwner>,
        tool_name: &str,
        args: &serde_json::Value,
        classification: Classification,
        raw: &CallToolResult,
        duration_ms: u64,
        started_at: chrono::DateTime<chrono::Utc>,
    ) -> Option<(CallToolResult, ToolInvocationId)> {
        let persisted = self
            .persist_classification(
                owner,
                tool_name,
                args,
                classification,
                raw.structured_content.clone(),
                duration_ms,
                started_at,
            )
            .await?;

        let invocation_id_str = uuid::Uuid::from(persisted.invocation_id).to_string();
        let mut summary_value = persisted.summary_value;
        substitute_recovery_placeholder(&mut summary_value, &invocation_id_str);

        let envelope = serde_json::json!({
            "invocation_id": invocation_id_str,
            "summary": summary_value,
        });

        let mut wrapped = raw.clone();
        wrapped.content = vec![Content::text(envelope_text(
            &persisted.summary,
            persisted.invocation_id,
            persisted.raw_size_bytes,
        ))];
        wrapped.structured_content = Some(envelope);
        Some((wrapped, persisted.invocation_id))
    }
}

pub struct PersistedClassification {
    pub invocation_id: ToolInvocationId,
    pub summary: ToolResultSummary,
    pub summary_value: serde_json::Value,
    pub raw_size_bytes: u64,
}

fn envelope_text(summary: &ToolResultSummary, id: ToolInvocationId, raw_size_bytes: u64) -> String {
    summary.render_envelope_text(&uuid::Uuid::from(id).to_string(), raw_size_bytes)
}

pub fn substitute_recovery_placeholder(value: &mut serde_json::Value, invocation_id: &str) {
    match value {
        serde_json::Value::String(s) if s == RECOVERY_INVOCATION_PLACEHOLDER => {
            *s = invocation_id.to_string();
        }
        serde_json::Value::Array(items) => {
            for item in items {
                substitute_recovery_placeholder(item, invocation_id);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values_mut() {
                substitute_recovery_placeholder(v, invocation_id);
            }
        }
        _ => {}
    }
}

pub fn apply_fetch_query(
    raw: &str,
    structured: Option<&serde_json::Value>,
    query: &FetchQuery,
) -> Result<FetchResult, ToolInvocationError> {
    let total_bytes = raw.len() as u64;

    let content = match query {
        FetchQuery::JsonPath { path } => {
            let root = structured.ok_or_else(|| {
                ToolInvocationError::InvalidPattern(
                    "json_path requested but invocation has no structured_content"
                        .to_string(),
                )
            })?;
            let value = resolve_json_path(root, path).ok_or_else(|| {
                ToolInvocationError::InvalidPattern(format!(
                    "json_path {path:?} did not resolve in structured_content"
                ))
            })?;
            serde_json::to_string_pretty(value).unwrap_or_default()
        }
        FetchQuery::JsonArraySlice { path, offset, len } => {
            let root = structured.ok_or_else(|| {
                ToolInvocationError::InvalidPattern(
                    "json_array_slice requested but invocation has no structured_content"
                        .to_string(),
                )
            })?;
            let value = resolve_json_path(root, path).ok_or_else(|| {
                ToolInvocationError::InvalidPattern(format!(
                    "json_array_slice path {path:?} did not resolve in structured_content"
                ))
            })?;
            let array = value.as_array().ok_or_else(|| {
                ToolInvocationError::InvalidPattern(format!(
                    "json_array_slice path {path:?} resolved to {} not an array",
                    json_kind_name(value)
                ))
            })?;
            let total = array.len();
            let start = (*offset as usize).min(total);
            let end = (start + *len as usize).min(total);
            let slice: Vec<&serde_json::Value> = array[start..end].iter().collect();
            serde_json::to_string_pretty(&slice).unwrap_or_default()
        }
        FetchQuery::Tail { lines } => {
            let n = *lines as usize;
            let collected: Vec<&str> = raw.lines().collect();
            let start = collected.len().saturating_sub(n);
            collected[start..].join("\n")
        }
        FetchQuery::Head { lines } => {
            let n = *lines as usize;
            raw.lines().take(n).collect::<Vec<&str>>().join("\n")
        }
        FetchQuery::Range { offset, len } => {
            let start = *offset as usize;
            if start >= raw.len() {
                String::new()
            } else {
                let end = (start + *len as usize).min(raw.len());
                let end = floor_char_boundary(raw, end);
                let start = floor_char_boundary(raw, start);
                raw[start..end].to_string()
            }
        }
        FetchQuery::Grep {
            pattern,
            case_insensitive,
            after_context,
            before_context,
            context,
            line_numbers,
            invert_match,
            head_limit,
        } => {
            if pattern.len() > MAX_GREP_PATTERN_LENGTH {
                return Err(ToolInvocationError::InvalidPattern(format!(
                    "grep pattern too long ({} chars, max {MAX_GREP_PATTERN_LENGTH})",
                    pattern.len()
                )));
            }
            let re = regex::RegexBuilder::new(pattern)
                .case_insensitive(*case_insensitive)
                .build()
                .map_err(|e| ToolInvocationError::InvalidPattern(format!("invalid regex: {e}")))?;
            let lines: Vec<&str> = raw.lines().collect();

            let (before, after) = match (before_context, after_context) {
                (None, None) => {
                    let c = context.map(|c| c as usize).unwrap_or(0);
                    (c, c)
                }
                (b, a) => (
                    b.map(|n| n as usize).unwrap_or(0),
                    a.map(|n| n as usize).unwrap_or(0),
                ),
            };

            let kept = grep::filter_lines_rich(grep::FilterArgs {
                lines: &lines,
                re: &re,
                invert: *invert_match,
                before,
                after,
                line_numbers: *line_numbers,
                head_limit: head_limit.map(|n| n as usize),
            });
            kept.join("\n")
        }
    };

    let (content, elision) = if content.len() > MAX_FETCH_RESPONSE_BYTES {
        let slice_bytes = content.len() as u64;
        let cut = floor_char_boundary(&content, MAX_FETCH_RESPONSE_BYTES);
        let kept = content[..cut].to_string();
        let hint = refine_hint(query);
        let trailer = format!(
            "\n[… {} bytes of a {} byte slice elided (response cap {}). \
             persisted raw is {} bytes — refine: {}]",
            slice_bytes - cut as u64,
            slice_bytes,
            MAX_FETCH_RESPONSE_BYTES,
            total_bytes,
            hint,
        );
        (
            format!("{kept}{trailer}"),
            Some(FetchElision {
                slice_bytes,
                kept_bytes: cut as u64,
                hint,
            }),
        )
    } else {
        (content, None)
    };

    Ok(FetchResult {
        content,
        total_bytes,
        elision,
    })
}

fn refine_hint(query: &FetchQuery) -> String {
    match query {
        FetchQuery::Tail { lines } => format!(
            "lower `lines` (currently {lines}); or switch to `range` for a known byte window"
        ),
        FetchQuery::Head { lines } => format!(
            "lower `lines` (currently {lines}); or switch to `range` for a known byte window"
        ),
        FetchQuery::Range { len, .. } => format!(
            "lower `len` (currently {len}); or switch to `head`/`tail` for an end of the data"
        ),
        FetchQuery::Grep {
            pattern,
            head_limit,
            before_context,
            after_context,
            context,
            ..
        } => {
            let mut parts: Vec<String> = Vec::new();
            match head_limit {
                None => parts.push("set `head_limit` (e.g. 50)".to_string()),
                Some(n) => parts.push(format!("lower `head_limit` (currently {n})")),
            }
            if before_context.is_some() || after_context.is_some() {
                parts.push("reduce `-A`/`-B` context".to_string());
            } else if let Some(c) = context {
                if *c > 0 {
                    parts.push(format!("reduce `-C` context (currently {c})"));
                }
            }
            parts.push(format!(
                "tighten the pattern (currently {pattern:?}) — anchors / specific keywords"
            ));
            parts.join("; ")
        }
        FetchQuery::JsonPath { path } => format!(
            "narrow `path` (currently {path:?}); or switch to `json_array_slice` for an array slice"
        ),
        FetchQuery::JsonArraySlice { path, len, .. } => format!(
            "lower `len` (currently {len}) at `path` {path:?}; or step the slice with `offset`"
        ),
    }
}

pub fn resolve_json_path<'a>(
    root: &'a serde_json::Value,
    path: &str,
) -> Option<&'a serde_json::Value> {
    let trimmed = path.trim();
    let trimmed = trimmed.strip_prefix('$').unwrap_or(trimmed);
    let trimmed = trimmed.strip_prefix('.').unwrap_or(trimmed);
    if trimmed.is_empty() {
        return Some(root);
    }
    let mut current = root;
    let mut segment = String::new();
    let mut chars = trimmed.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '.' => {
                if !segment.is_empty() {
                    current = current.get(segment.as_str())?;
                    segment.clear();
                }
            }
            '[' => {
                if !segment.is_empty() {
                    current = current.get(segment.as_str())?;
                    segment.clear();
                }
                let mut idx = String::new();
                let mut closed = false;
                while let Some(&c) = chars.peek() {
                    if c == ']' {
                        chars.next();
                        closed = true;
                        break;
                    }
                    idx.push(c);
                    chars.next();
                }
                if !closed {
                    return None;
                }
                let i: usize = idx.parse().ok()?;
                current = current.get(i)?;
            }
            _ => segment.push(c),
        }
    }
    if !segment.is_empty() {
        current = current.get(segment.as_str())?;
    }
    Some(current)
}

fn json_kind_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

// `str::floor_char_boundary` is unstable on stable Rust.
fn floor_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let mut i = idx;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> &'static str {
        "alpha\nbravo error one\ncharlie\ndelta error two\necho\nfoxtrot"
    }

    #[test]
    fn tail_returns_last_n_lines() {
        let r = apply_fetch_query(sample(), None, &FetchQuery::Tail { lines: 2 }).unwrap();
        assert_eq!(r.content, "echo\nfoxtrot");
        assert!(r.elision.is_none());
        assert_eq!(r.total_bytes, sample().len() as u64);
    }

    #[test]
    fn head_returns_first_n_lines() {
        let r = apply_fetch_query(sample(), None, &FetchQuery::Head { lines: 1 }).unwrap();
        assert_eq!(r.content, "alpha");
    }

    #[test]
    fn range_slices_bytes() {
        let r = apply_fetch_query(sample(), None, &FetchQuery::Range { offset: 0, len: 5 }).unwrap();
        assert_eq!(r.content, "alpha");
    }

    fn grep_pattern(pattern: &str) -> FetchQuery {
        FetchQuery::Grep {
            pattern: pattern.to_string(),
            case_insensitive: false,
            after_context: None,
            before_context: None,
            context: None,
            line_numbers: false,
            invert_match: false,
            head_limit: None,
        }
    }

    #[test]
    fn grep_matches_lines() {
        let r = apply_fetch_query(sample(), None, &grep_pattern("error")).unwrap();
        assert_eq!(r.content, "bravo error one\ndelta error two");
    }

    #[test]
    fn grep_with_context_includes_neighbours() {
        let mut q = grep_pattern("error one");
        if let FetchQuery::Grep { context, .. } = &mut q {
            *context = Some(1);
        }
        let r = apply_fetch_query(sample(), None, &q).unwrap();
        assert_eq!(r.content, "alpha\nbravo error one\ncharlie");
    }

    #[test]
    fn grep_invalid_regex_errors() {
        let err = apply_fetch_query(sample(), None, &grep_pattern("[invalid")).unwrap_err();
        assert!(matches!(err, ToolInvocationError::InvalidPattern(_)));
    }

    #[test]
    fn grep_case_insensitive_flag() {
        let mut q = grep_pattern("ERROR");
        if let FetchQuery::Grep {
            case_insensitive, ..
        } = &mut q
        {
            *case_insensitive = true;
        }
        let r = apply_fetch_query(sample(), None, &q).unwrap();
        assert_eq!(r.content, "bravo error one\ndelta error two");
    }

    #[test]
    fn grep_invert_match_drops_matches() {
        let mut q = grep_pattern("error");
        if let FetchQuery::Grep { invert_match, .. } = &mut q {
            *invert_match = true;
        }
        let r = apply_fetch_query(sample(), None, &q).unwrap();
        assert_eq!(r.content, "alpha\ncharlie\necho\nfoxtrot");
    }

    #[test]
    fn grep_line_numbers_prefix_kept_lines() {
        let mut q = grep_pattern("error");
        if let FetchQuery::Grep { line_numbers, .. } = &mut q {
            *line_numbers = true;
        }
        let r = apply_fetch_query(sample(), None, &q).unwrap();
        assert_eq!(r.content, "2:bravo error one\n4:delta error two");
    }

    #[test]
    fn grep_asymmetric_context_after_only() {
        let mut q = grep_pattern("error one");
        if let FetchQuery::Grep { after_context, .. } = &mut q {
            *after_context = Some(1);
        }
        let r = apply_fetch_query(sample(), None, &q).unwrap();
        assert_eq!(r.content, "bravo error one\ncharlie");
    }

    #[test]
    fn grep_head_limit_caps_kept_lines() {
        let mut q = grep_pattern("error");
        if let FetchQuery::Grep { head_limit, .. } = &mut q {
            *head_limit = Some(1);
        }
        let r = apply_fetch_query(sample(), None, &q).unwrap();
        assert_eq!(r.content, "bravo error one");
    }

    #[test]
    fn oversize_slice_returns_head_with_elision_metadata() {
        let big = "x".repeat(MAX_FETCH_RESPONSE_BYTES + 1024);
        let r = apply_fetch_query(
            &big,
            None,
            &FetchQuery::Range {
                offset: 0,
                len: u32::MAX,
            },
        )
        .unwrap();
        let elision = r.elision.expect("oversize slice produces elision metadata");
        assert_eq!(elision.kept_bytes, MAX_FETCH_RESPONSE_BYTES as u64);
        assert_eq!(elision.slice_bytes, big.len() as u64);
        assert!(elision.hint.contains("`len`"));
        assert!(r.content.starts_with(&"x".repeat(MAX_FETCH_RESPONSE_BYTES)));
        assert!(r.content.contains("byte slice elided"));
        assert!(r.content.contains("refine:"));
    }

    #[test]
    fn refine_hint_grep_unset_head_limit_suggests_setting_one() {
        let q = FetchQuery::Grep {
            pattern: "err".into(),
            case_insensitive: false,
            after_context: None,
            before_context: None,
            context: None,
            line_numbers: false,
            invert_match: false,
            head_limit: None,
        };
        let h = refine_hint(&q);
        assert!(h.contains("set `head_limit`"));
        assert!(h.contains("tighten the pattern"));
    }

    #[test]
    fn refine_hint_grep_with_context_suggests_reducing() {
        let q = FetchQuery::Grep {
            pattern: "err".into(),
            case_insensitive: false,
            after_context: Some(3),
            before_context: Some(2),
            context: None,
            line_numbers: false,
            invert_match: false,
            head_limit: Some(100),
        };
        let h = refine_hint(&q);
        assert!(h.contains("lower `head_limit`"));
        assert!(h.contains("`-A`/`-B`"));
    }

    #[test]
    fn floor_char_boundary_handles_multibyte() {
        let s = "ab\u{1F600}cd";
        assert_eq!(floor_char_boundary(s, 0), 0);
        assert_eq!(floor_char_boundary(s, 2), 2);
        assert_eq!(floor_char_boundary(s, 3), 2);
        assert_eq!(floor_char_boundary(s, 6), 6);
        assert_eq!(floor_char_boundary(s, 100), s.len());
    }

    #[test]
    fn resolve_json_path_root() {
        let v = serde_json::json!({"a": 1});
        assert_eq!(resolve_json_path(&v, "$"), Some(&v));
        assert_eq!(resolve_json_path(&v, ""), Some(&v));
    }

    #[test]
    fn resolve_json_path_nested_keys_and_index() {
        let v = serde_json::json!({"hits": [{"id": 1}, {"id": 2}, {"id": 3}]});
        assert_eq!(
            resolve_json_path(&v, "$.hits[1].id"),
            Some(&serde_json::json!(2))
        );
        assert_eq!(
            resolve_json_path(&v, "hits[0]"),
            Some(&serde_json::json!({"id": 1}))
        );
    }

    #[test]
    fn resolve_json_path_missing_returns_none() {
        let v = serde_json::json!({"a": 1});
        assert!(resolve_json_path(&v, "$.b").is_none());
        assert!(resolve_json_path(&v, "$.a[0]").is_none());
    }

    #[test]
    fn json_path_query_returns_value_at_path() {
        let structured = serde_json::json!({"user": {"name": "alice", "id": 42}});
        let r = apply_fetch_query(
            "irrelevant raw",
            Some(&structured),
            &FetchQuery::JsonPath {
                path: "$.user.name".into(),
            },
        )
        .unwrap();
        assert_eq!(r.content.trim(), "\"alice\"");
    }

    #[test]
    fn json_array_slice_returns_requested_range() {
        let structured = serde_json::json!({
            "hits": [
                {"id": 0}, {"id": 1}, {"id": 2}, {"id": 3}, {"id": 4},
                {"id": 5}, {"id": 6}, {"id": 7}, {"id": 8}, {"id": 9}
            ]
        });
        let r = apply_fetch_query(
            "irrelevant raw",
            Some(&structured),
            &FetchQuery::JsonArraySlice {
                path: "$.hits".into(),
                offset: 3,
                len: 4,
            },
        )
        .unwrap();
        assert!(r.content.contains("\"id\": 3"));
        assert!(r.content.contains("\"id\": 6"));
        assert!(!r.content.contains("\"id\": 7"));
        assert!(!r.content.contains("\"id\": 2"));
    }

    #[test]
    fn json_array_slice_clamps_to_array_length() {
        let structured = serde_json::json!({"hits": [0, 1, 2]});
        let r = apply_fetch_query(
            "irrelevant raw",
            Some(&structured),
            &FetchQuery::JsonArraySlice {
                path: "$.hits".into(),
                offset: 1,
                len: 100,
            },
        )
        .unwrap();
        assert!(r.content.contains('1'));
        assert!(r.content.contains('2'));
    }

    #[test]
    fn json_path_without_structured_errors_helpfully() {
        let err = apply_fetch_query(
            "raw text only",
            None,
            &FetchQuery::JsonPath {
                path: "$.foo".into(),
            },
        )
        .unwrap_err();
        let s = err.to_string().to_lowercase();
        assert!(s.contains("structured_content"));
    }

    #[test]
    fn json_array_slice_on_non_array_errors() {
        let structured = serde_json::json!({"foo": "not an array"});
        let err = apply_fetch_query(
            "raw text only",
            Some(&structured),
            &FetchQuery::JsonArraySlice {
                path: "$.foo".into(),
                offset: 0,
                len: 10,
            },
        )
        .unwrap_err();
        let s = err.to_string().to_lowercase();
        assert!(s.contains("not an array") || s.contains("string"));
    }
}
