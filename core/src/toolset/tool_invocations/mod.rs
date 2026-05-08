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
mod grep;
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
    /// Line-grep with rg-style flags. Flag spelling mirrors the top-level
    /// `Grep` tool so the model can transfer the shape directly. Operates
    /// over the persisted `raw_text` line-by-line; cross-line patterns
    /// are not currently supported (no `--multiline-dotall` equivalent).
    Grep {
        /// Regular expression pattern matched against each line.
        pattern: String,

        /// Case-insensitive search (rg's `-i`). Equivalent to prefixing
        /// `pattern` with `(?i)`, but explicit for discoverability.
        #[serde(rename = "-i", default)]
        case_insensitive: bool,

        /// Lines of context after each match (rg's `-A`).
        #[serde(rename = "-A", default, skip_serializing_if = "Option::is_none")]
        after_context: Option<u32>,

        /// Lines of context before each match (rg's `-B`).
        #[serde(rename = "-B", default, skip_serializing_if = "Option::is_none")]
        before_context: Option<u32>,

        /// Symmetric pre/post context — equivalent to setting both
        /// `-A` and `-B` to the same value (rg's `-C`). Ignored when
        /// either `-A` or `-B` is set.
        #[serde(rename = "-C", default, skip_serializing_if = "Option::is_none")]
        context: Option<u32>,

        /// Prefix kept lines with their 1-based line number from the
        /// original `raw_text` (rg's `-n`). Useful as input to a later
        /// `range`/`head`/`tail` query against the same invocation.
        /// Default: true.
        #[serde(rename = "-n", default = "default_true")]
        line_numbers: bool,

        /// Drop matches; keep non-matching lines (rg's `-v`).
        #[serde(default)]
        invert_match: bool,

        /// Cap output to first N kept lines (after context expansion).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        head_limit: Option<u32>,
    },
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct FetchResult {
    /// Model-facing slice. When the slice exceeded the response cap,
    /// the head fits and a `[… N bytes elided …]` trailer is appended
    /// with refinement guidance — `elision` carries the same info
    /// structurally for programmatic callers.
    pub content: String,

    /// Total size of the persisted `raw_text` for this invocation, in bytes.
    /// Useful for budgeting follow-up `range` queries. Not the size of
    /// `content` — `content` is the slice you asked for.
    pub total_bytes: u64,

    /// `Some(...)` when the slice the query produced exceeded the
    /// response cap. The persisted invocation is the canonical source
    /// of truth, so we do NOT persist a new row for the elided slice;
    /// agents refine against the original `invocation_id` instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elision: Option<FetchElision>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct FetchElision {
    /// Size in bytes the slice would have been if uncapped.
    pub slice_bytes: u64,
    /// Size in bytes actually returned in `content` (excluding trailer).
    pub kept_bytes: u64,
    /// Per-mode advice for narrowing the next query so the slice fits.
    pub hint: String,
}

/// Maximum length of a `Grep` regex.
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
        ToolResultSummary::ConcourseLogs(s) => {
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
                // Nested classification of the failure's log_tail —
                // typed shape (e.g. `nix_build`) inlined under the
                // failure block. Recursion bottoms out fast (parent
                // already capped log_tail at MAX_FAILURE_LOG_TAIL).
                if let Some(embedded) = &f.embedded {
                    out.push_str(&format!("embedded ({}):\n", embedded.kind()));
                    let nested = envelope_text(embedded, id);
                    for line in nested.lines() {
                        out.push_str("  ");
                        out.push_str(line);
                        out.push('\n');
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
        ToolResultSummary::NixBuild(s) => {
            let mut out = String::new();
            out.push_str(&format!(
                "[nix build: {} derivations / {} cache copies / {} failures; \
                 {} bytes raw; fetch the full stream via \
                 tool_output_fetch(invocation_id=\"{}\")]\n",
                s.derivations_attempted,
                s.cache_paths_copied,
                s.failures.len(),
                s.total_bytes,
                id,
            ));
            for f in &s.failures {
                out.push_str(&format!("=== failure: {} ===\n", f.drv_path));
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

            // -A / -B win over -C when either is set; otherwise -C
            // applies symmetrically; otherwise no context.
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

/// Per-mode advice for narrowing a follow-up query when the previous
/// slice exceeded the response cap. The persisted raw at the parent
/// `invocation_id` is the source of truth — refinement always goes
/// back to it, never to a slice-of-a-slice (which we deliberately
/// don't persist).
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
    }
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
        assert!(r.elision.is_none());
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
        let r = apply_fetch_query(sample(), &grep_pattern("error")).unwrap();
        assert_eq!(r.content, "bravo error one\ndelta error two");
    }

    #[test]
    fn grep_with_context_includes_neighbours() {
        let mut q = grep_pattern("error one");
        if let FetchQuery::Grep { context, .. } = &mut q {
            *context = Some(1);
        }
        let r = apply_fetch_query(sample(), &q).unwrap();
        assert_eq!(r.content, "alpha\nbravo error one\ncharlie");
    }

    #[test]
    fn grep_invalid_regex_errors() {
        let err = apply_fetch_query(sample(), &grep_pattern("[invalid")).unwrap_err();
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
        let r = apply_fetch_query(sample(), &q).unwrap();
        assert_eq!(r.content, "bravo error one\ndelta error two");
    }

    #[test]
    fn grep_invert_match_drops_matches() {
        let mut q = grep_pattern("error");
        if let FetchQuery::Grep { invert_match, .. } = &mut q {
            *invert_match = true;
        }
        let r = apply_fetch_query(sample(), &q).unwrap();
        assert_eq!(r.content, "alpha\ncharlie\necho\nfoxtrot");
    }

    #[test]
    fn grep_line_numbers_prefix_kept_lines() {
        let mut q = grep_pattern("error");
        if let FetchQuery::Grep { line_numbers, .. } = &mut q {
            *line_numbers = true;
        }
        let r = apply_fetch_query(sample(), &q).unwrap();
        assert_eq!(r.content, "2:bravo error one\n4:delta error two");
    }

    #[test]
    fn grep_asymmetric_context_after_only() {
        let mut q = grep_pattern("error one");
        if let FetchQuery::Grep { after_context, .. } = &mut q {
            *after_context = Some(1);
        }
        let r = apply_fetch_query(sample(), &q).unwrap();
        assert_eq!(r.content, "bravo error one\ncharlie");
    }

    #[test]
    fn grep_head_limit_caps_kept_lines() {
        let mut q = grep_pattern("error");
        if let FetchQuery::Grep { head_limit, .. } = &mut q {
            *head_limit = Some(1);
        }
        let r = apply_fetch_query(sample(), &q).unwrap();
        assert_eq!(r.content, "bravo error one");
    }

    #[test]
    fn oversize_slice_returns_head_with_elision_metadata() {
        let big = "x".repeat(MAX_FETCH_RESPONSE_BYTES + 1024);
        let r = apply_fetch_query(
            &big,
            &FetchQuery::Range {
                offset: 0,
                len: u32::MAX,
            },
        )
        .unwrap();
        let elision = r
            .elision
            .expect("oversize slice should produce elision metadata");
        assert_eq!(elision.kept_bytes, MAX_FETCH_RESPONSE_BYTES as u64);
        assert_eq!(elision.slice_bytes, big.len() as u64);
        assert!(elision.hint.contains("`len`"));
        // content carries the kept head followed by the trailer.
        assert!(r.content.starts_with(&"x".repeat(MAX_FETCH_RESPONSE_BYTES)));
        assert!(r.content.contains("bytes elided"));
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
        assert!(h.contains("set `head_limit`"), "got: {h}");
        assert!(h.contains("tighten the pattern"), "got: {h}");
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
        assert!(h.contains("lower `head_limit`"), "got: {h}");
        assert!(h.contains("`-A`/`-B`"), "got: {h}");
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
