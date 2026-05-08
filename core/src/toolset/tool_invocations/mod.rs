//! Persistent layer for the tool-output universal pipeline. The dispatcher
//! calls into [`ToolInvocations`] when a [`ResultClassifier`] emits anything
//! other than `Passthrough`; the captured raw output is persisted so the
//! agent can recover detail later via the `tool_output_fetch` MCP tool.
//!
//! Boilerplate scope (issue 019e01c5): no hot-tier in-process cache, no TTL
//! sweeper, no compression. The PG row is the single source of truth and is
//! cheap enough that every `tool_output_fetch` is a SELECT round-trip.
//!
//! [`ResultClassifier`]: super::classifier::ResultClassifier

mod entity;
mod error;
mod repo;

pub use entity::{NewToolInvocation, ToolInvocation};
pub use error::ToolInvocationError;

use serde::{Deserialize, Serialize};
use tracing::instrument;

use crate::primitives::{AgentId, ToolInvocationId};

use repo::ToolInvocationRepo;

/// What the agent asked to retrieve from a persisted invocation. Exactly one
/// mode at a time — kept tagged so the wire schema is unambiguous.
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
    Grep {
        pattern: String,
        #[serde(default)]
        context: Option<u32>,
    },
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct FetchResult {
    pub content: String,
    pub truncated: bool,
    pub total_bytes: u64,
}

/// Maximum length of a `Grep` regex. Mirrors `OutputFilter`'s cap.
const MAX_GREP_PATTERN_LENGTH: usize = 1000;

/// Hard ceiling on the bytes a single `tool_output_fetch` may return. The
/// fetch escape hatch is meant to surface targeted detail, not to ship the
/// whole blob back through the model context — that defeats the elision.
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

    #[instrument(name = "core.tool_invocations.persist", skip_all)]
    pub async fn persist(
        &self,
        new: NewToolInvocation,
    ) -> Result<ToolInvocation, ToolInvocationError> {
        Ok(self.repo.create(new).await?)
    }

    #[instrument(name = "core.tool_invocations.find_by_id", skip(self))]
    pub async fn find_by_id(
        &self,
        id: ToolInvocationId,
    ) -> Result<ToolInvocation, ToolInvocationError> {
        Ok(self.repo.find_by_id(id).await?)
    }

    #[instrument(name = "core.tool_invocations.fetch", skip(self))]
    pub async fn fetch(
        &self,
        id: ToolInvocationId,
        query: FetchQuery,
    ) -> Result<FetchResult, ToolInvocationError> {
        let invocation = self.repo.find_by_id(id).await?;
        apply_fetch_query(&invocation.raw_text, &query)
    }

    /// Cache-aware diff probe. Stub for the boilerplate — the consuming
    /// `Diff` summary variant lands in a follow-up PR. The
    /// `(agent_id, args_hash)` index is in place so the eventual query is
    /// a single index lookup.
    #[instrument(name = "core.tool_invocations.find_for_diff", skip(self))]
    pub async fn find_for_diff(
        &self,
        agent_id: AgentId,
        args_hash: &[u8],
    ) -> Result<Option<ToolInvocation>, ToolInvocationError> {
        Ok(self
            .repo
            .find_latest_by_args_hash(agent_id, args_hash)
            .await?)
    }
}

fn apply_fetch_query(raw: &str, query: &FetchQuery) -> Result<FetchResult, ToolInvocationError> {
    let total_bytes = raw.len() as u64;

    let content = match query {
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
                // Snap to a UTF-8 char boundary so the slice is valid utf8.
                let end = floor_char_boundary(raw, end);
                let start = floor_char_boundary(raw, start);
                raw[start..end].to_string()
            }
        }
        FetchQuery::Grep { pattern, context } => {
            if pattern.len() > MAX_GREP_PATTERN_LENGTH {
                return Err(ToolInvocationError::InvalidPattern(format!(
                    "grep pattern too long ({} chars, max {MAX_GREP_PATTERN_LENGTH})",
                    pattern.len()
                )));
            }
            let re = regex::Regex::new(pattern)
                .map_err(|e| ToolInvocationError::InvalidPattern(format!("invalid regex: {e}")))?;
            let lines: Vec<&str> = raw.lines().collect();
            // Reuse the in-tree `filter::filter_lines` helper (grep + context).
            let kept = super::filter::filter_lines(
                &lines,
                &re,
                /* invert */ false,
                context.map(|c| c as usize),
            );
            kept.join("\n")
        }
    };

    let (content, truncated) = if content.len() > MAX_FETCH_RESPONSE_BYTES {
        let cut = floor_char_boundary(&content, MAX_FETCH_RESPONSE_BYTES);
        (content[..cut].to_string(), true)
    } else {
        (content, false)
    };

    Ok(FetchResult {
        content,
        truncated,
        total_bytes,
    })
}

/// `str::floor_char_boundary` is unstable on stable Rust 1.x. Reimplement it
/// here so byte-offset truncation never panics on a multi-byte UTF-8 boundary.
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
        let r = apply_fetch_query(sample(), &FetchQuery::Tail { lines: 2 }).unwrap();
        assert_eq!(r.content, "echo\nfoxtrot");
        assert!(!r.truncated);
        assert_eq!(r.total_bytes, sample().len() as u64);
    }

    #[test]
    fn head_returns_first_n_lines() {
        let r = apply_fetch_query(sample(), &FetchQuery::Head { lines: 1 }).unwrap();
        assert_eq!(r.content, "alpha");
    }

    #[test]
    fn range_slices_bytes() {
        let r = apply_fetch_query(sample(), &FetchQuery::Range { offset: 0, len: 5 }).unwrap();
        assert_eq!(r.content, "alpha");
    }

    #[test]
    fn grep_matches_lines() {
        let r = apply_fetch_query(
            sample(),
            &FetchQuery::Grep {
                pattern: "error".to_string(),
                context: None,
            },
        )
        .unwrap();
        assert_eq!(r.content, "bravo error one\ndelta error two");
    }

    #[test]
    fn grep_with_context_includes_neighbours() {
        let r = apply_fetch_query(
            sample(),
            &FetchQuery::Grep {
                pattern: "error one".to_string(),
                context: Some(1),
            },
        )
        .unwrap();
        assert_eq!(r.content, "alpha\nbravo error one\ncharlie");
    }

    #[test]
    fn grep_invalid_regex_errors() {
        let err = apply_fetch_query(
            sample(),
            &FetchQuery::Grep {
                pattern: "[invalid".to_string(),
                context: None,
            },
        )
        .unwrap_err();
        assert!(matches!(err, ToolInvocationError::InvalidPattern(_)));
    }

    #[test]
    fn fetch_response_capped_at_max_bytes() {
        let big = "x".repeat(MAX_FETCH_RESPONSE_BYTES + 1024);
        let r = apply_fetch_query(
            &big,
            &FetchQuery::Range {
                offset: 0,
                len: u32::MAX,
            },
        )
        .unwrap();
        assert!(r.truncated);
        assert_eq!(r.content.len(), MAX_FETCH_RESPONSE_BYTES);
    }

    #[test]
    fn floor_char_boundary_handles_multibyte() {
        let s = "ab\u{1F600}cd";
        assert_eq!(floor_char_boundary(s, 0), 0);
        assert_eq!(floor_char_boundary(s, 2), 2);
        // mid-emoji index snaps down
        assert_eq!(floor_char_boundary(s, 3), 2);
        assert_eq!(floor_char_boundary(s, 6), 6);
        assert_eq!(floor_char_boundary(s, 100), s.len());
    }
}
