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

pub use entity::{NewToolInvocation, ToolInvocation, ToolInvocationOwner};
pub use error::ToolInvocationError;

use rmcp::model::{CallToolResult, Content};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::instrument;

use crate::audit::Audit;
use crate::auth::AuthSubject;
use crate::primitives::ToolInvocationId;

use super::classifier::{Classification, ToolResultSummary};
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

    /// Cache-aware diff probe — most-recent invocation matching
    /// `(owner, args_hash)`. The consuming `Diff` summary variant lands
    /// in a follow-up PR; the `(scope, args_hash)` partial indexes are
    /// in place so each lookup is a single index hit.
    #[instrument(name = "core.tool_invocations.find_for_diff", skip(self))]
    pub async fn find_for_diff(
        &self,
        owner: ToolInvocationOwner,
        args_hash: &[u8],
    ) -> Result<Option<ToolInvocation>, ToolInvocationError> {
        Ok(self.repo.find_latest_by_args_hash(owner, args_hash).await?)
    }

    /// Persist the classifier's output as a `tool_invocations` row and
    /// record the resulting id on the audit row. Pure persistence — no
    /// CallToolResult mutation, no envelope construction. Used by both
    /// `persist_and_envelope` (which wraps the model-facing result
    /// after) and compose's CatalogDispatcher (which tracks the
    /// invocation_id in `sub_invocations` while leaving the JS-facing
    /// result un-wrapped).
    ///
    /// Returns `None` when:
    /// - the subject doesn't yield an owner (`Anonymous`,
    ///   `WorkflowExecutor` — see `ToolInvocationOwner::from_subject`);
    /// - summary or args fail to serialize (logged + skipped);
    /// - the PG insert errors (logged + skipped).
    #[instrument(name = "core.tool_invocations.persist_classification", skip_all)]
    #[allow(clippy::too_many_arguments)]
    pub async fn persist_classification(
        &self,
        subject: &AuthSubject,
        tool_name: &str,
        args: &serde_json::Value,
        classification: Classification,
        original_structured: Option<serde_json::Value>,
        duration_ms: u64,
        started_at: chrono::DateTime<chrono::Utc>,
    ) -> Option<PersistedClassification> {
        let owner = ToolInvocationOwner::from_subject(subject)?;
        let Classification {
            summary,
            canonical_text,
        } = classification;
        // `canonical_text` IS the bytes the summary's offsets point at —
        // persist exactly those so subsequent `tool_output_fetch` calls
        // return text whose line numbers match the summary's slicing.
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

        Audit::record_tool_invocation_id(persisted.id);

        Some(PersistedClassification {
            invocation_id: persisted.id,
            summary,
            summary_value,
            raw_size_bytes: raw_size_bytes as u64,
        })
    }

    /// Persist the captured raw output and decorate the original
    /// `CallToolResult` with an `envelope.invocation_id` so the caller
    /// can recover detail through `tool_output_fetch`. Returns the
    /// original raw result on persistence failure (the model still
    /// gets a usable result; the failure is logged).
    #[instrument(name = "core.tool_invocations.persist_and_envelope", skip_all)]
    #[allow(clippy::too_many_arguments)]
    pub async fn persist_and_envelope(
        &self,
        subject: &AuthSubject,
        tool_name: &str,
        args: &serde_json::Value,
        classification: Classification,
        raw: &CallToolResult,
        duration_ms: u64,
        started_at: chrono::DateTime<chrono::Utc>,
    ) -> Option<CallToolResult> {
        let persisted = self
            .persist_classification(
                subject,
                tool_name,
                args,
                classification,
                raw.structured_content.clone(),
                duration_ms,
                started_at,
            )
            .await?;

        // No `fetch_hint` here — `tool_output_fetch` is a visible
        // top-level tool, so its `description()` already carries the
        // call shape and per-mode args. The terse `fetch via
        // tool_output_fetch(invocation_id="…")` line emitted by
        // `envelope_text` is enough to point the agent at recovery;
        // duplicating the schema in every persisted envelope is
        // pure context noise.
        let envelope = serde_json::json!({
            "invocation_id": uuid::Uuid::from(persisted.invocation_id).to_string(),
            "summary": persisted.summary_value,
        });

        let mut wrapped = raw.clone();
        wrapped.content = vec![Content::text(envelope_text(
            &persisted.summary,
            persisted.invocation_id,
        ))];
        wrapped.structured_content = Some(envelope);
        Some(wrapped)
    }
}

/// Result of [`ToolInvocations::persist_classification`]. Carries the
/// pieces both the dispatcher (envelope construction) and compose
/// (sub_invocations directory) need to inspect after persistence —
/// without re-classifying or re-loading the persisted row.
pub struct PersistedClassification {
    pub invocation_id: ToolInvocationId,
    pub summary: ToolResultSummary,
    pub summary_value: serde_json::Value,
    pub raw_size_bytes: u64,
}

fn envelope_text(summary: &ToolResultSummary, id: ToolInvocationId) -> String {
    match summary {
        ToolResultSummary::Passthrough { value } => match value {
            // Plain-text tools land here as Value::String — emit the
            // raw string. Structured tools land as a Value; pretty-
            // print so the model can read it.
            serde_json::Value::String(s) => s.clone(),
            other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
        },
        ToolResultSummary::StructuredElision {
            kept,
            elided_paths,
            total_bytes,
            kept_bytes,
        } => {
            // Pretty-printed `kept` so agents/humans read structure
            // naturally; the elided_paths list is rendered as a
            // structured table so the agent can scan what was
            // dropped without recursing through `kept` to find
            // sentinels.
            let mut out = String::new();
            out.push_str(&format!(
                "[json elided: {kept_bytes}/{total_bytes} bytes kept; \
                 fetch raw via tool_output_fetch(invocation_id=\"{id}\")]\n",
            ));
            if !elided_paths.is_empty() {
                out.push_str("=== elided paths ===\n");
                for p in elided_paths {
                    let kind_str = match p.kind {
                        crate::toolset::ElisionKind::String => "string",
                        crate::toolset::ElisionKind::Array => "array",
                        crate::toolset::ElisionKind::Object => "object",
                    };
                    let length_str = p
                        .length
                        .map(|n| format!(", length: {n}"))
                        .unwrap_or_default();
                    out.push_str(&format!(
                        "  {} ({}, {} bytes{})\n",
                        p.path, kind_str, p.bytes, length_str,
                    ));
                }
            }
            out.push_str("=== kept ===\n");
            let pretty = serde_json::to_string_pretty(kept).unwrap_or_else(|_| kept.to_string());
            out.push_str(&pretty);
            out
        }
        ToolResultSummary::Concourse(s) => {
            let mut out = String::new();
            out.push_str(&format!(
                "[concourse build log: status={:?}, {} lines / {} bytes; \
                 fetch the raw stream via tool_output_fetch(invocation_id=\"{}\")]\n",
                s.status, s.total_lines, s.total_bytes, id
            ));
            out.push_str(&format!(
                "tasks: {} | nix paths copied: {} | derivations: {} | \
                 cache files pruned: {}\n",
                s.task_phases.len(),
                s.nix_paths_copied,
                s.derivations_checked,
                s.cache_files_pruned,
            ));
            if !s.warnings.is_empty() {
                out.push_str("=== warnings ===\n");
                for w in &s.warnings {
                    out.push_str(&format!("[{}] {}\n", w.timestamp, w.message));
                }
            }
            if !s.errors.is_empty() {
                out.push_str("=== errors ===\n");
                for e in &s.errors {
                    out.push_str(&format!("[{}] {}\n", e.timestamp, e.message));
                }
            }
            for f in &s.failures {
                out.push_str(&format!("=== failure: {} ===\n", f.attribute,));
                if let Some(reason) = &f.reason {
                    out.push_str(&format!("reason: {reason}\n"));
                }
                if !f.log_tail.is_empty() {
                    out.push_str("log_tail:\n");
                    for l in &f.log_tail {
                        out.push_str(&format!("  > {l}\n"));
                    }
                }
            }
            if !s.final_lines.is_empty() {
                out.push_str("=== final lines ===\n");
                for l in &s.final_lines {
                    out.push_str(l);
                    out.push('\n');
                }
            }
            out
        }
    }
}

pub(crate) fn apply_fetch_query(
    raw: &str,
    query: &FetchQuery,
) -> Result<FetchResult, ToolInvocationError> {
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
