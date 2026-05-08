//! `ConcourseBuildLogClassifier` — typed summary for `concourse_get_build_logs`.
//!
//! Concourse jobs return plain timestamped text. The dominant noise sources
//! are:
//!   1. Nix substituter chatter — `copying path '/nix/store/...'` (often
//!      hundreds of lines on a cold cache).
//!   2. Per-derivation pipeline progress — `checking derivation ...`,
//!      `building '/nix/store/...drv'`, `derivation evaluated to ...`.
//!   3. Tail-end cache pruning — `nix-cache: removing /nix-cache/...`.
//!
//! Roughly 80–90% of a typical drua CI build log is one of those three.
//!
//! The agent-actionable signal is much smaller:
//!   - `warning:` lines (kept verbatim, with timestamps).
//!   - `error: failed to build attribute '...'` blocks (the failed
//!     derivation, its reason, and the indented `> ...` log lines that
//!     follow). These are nix's own "Last 25 log lines:" tail and contain
//!     the actual rust/clippy/test diagnostic.
//!   - Task phase markers (`=== with-nix-cache: start/setup done/done ===`).
//!   - The closing N lines verbatim — a defence-in-depth tail in case the
//!     classifier missed something.
//!
//! The summary collapses everything else to counters.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};

use super::string_classifier::StringClassifierChain;
use super::walker::{self, WalkOutcome};
use super::{
    Classification, ClassifierContext, ClassifierError, ResultClassifier, ToolResultSummary,
    DEFAULT_GENERIC_THRESHOLD_BYTES,
};

/// Lines kept from the very end of the log unconditionally. Bounded so a
/// pathological tail of cache-pruning still doesn't blow context, but big
/// enough to almost always include any final summary line.
const FINAL_TAIL_LINES: usize = 30;

/// Defence: a deeply-pathological log shouldn't allocate a million
/// warnings / errors into the summary.
const MAX_WARNINGS_KEPT: usize = 50;
const MAX_ERRORS_KEPT: usize = 50;

/// What the classifier could determine about the build's outcome from
/// the log alone. Deliberately NOT a `Succeeded` variant — concourse
/// builds don't emit a positive "the whole thing passed" line, so the
/// agent has to pair this with `concourse_get_build_status` (which knows
/// the actual exit code) when the call really matters. The classifier
/// only commits to "yes there's a failure block" or "no failure
/// patterns matched"; the latter is *not* the same as success.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConcourseBuildStatus {
    /// At least one structured failure was detected — either a nix
    /// `error: failed to build attribute '...'` block (captured into
    /// `failures`) or a stray `error:` line (captured into `errors`).
    /// The agent should treat the build as broken until proven
    /// otherwise.
    Failed,
    /// No failure pattern matched. Does NOT imply the build passed —
    /// shell-script errors, task timeouts, resource-step failures, and
    /// runner crashes all leave logs without a recognised failure
    /// signature. Pair with `concourse_get_build_status` for ground
    /// truth.
    NoFailureDetected,
    /// Empty log. Almost always means the build is in-flight or the API
    /// returned before any task started.
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TimestampedLine {
    /// `HH:MM:SS` extracted from the leading `[HH:MM:SS]` prefix, when
    /// present. Empty string when the line had no timestamp.
    pub timestamp: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConcourseBuildLogSummary {
    pub status: ConcourseBuildStatus,
    /// Names from the `=== with-nix-cache: <name> ===` task markers.
    pub task_phases: Vec<String>,
    pub warnings: Vec<TimestampedLine>,
    /// Stray `error:` lines that didn't open a structured nix failure
    /// block — shell-script errors, ad-hoc bash scripts emitting
    /// `error: foo`, build-task wrapper diagnostics. Each such line
    /// also escalates `status` to `Failed`.
    pub errors: Vec<TimestampedLine>,
    /// Always-on tail — last [`FINAL_TAIL_LINES`] lines verbatim.
    pub final_lines: Vec<String>,
    pub total_lines: u32,
    pub total_bytes: u64,
    /// Approximate number of bytes the agent now sees vs the raw input.
    /// `total_bytes / kept_bytes` is the compression ratio the inspector
    /// renders.
    pub kept_bytes: u32,
    /// Result of running the walker chain over the raw log content.
    /// Typically a `NixStringClassifier` typed sentinel
    /// (`{"_typed": "nix_build", "summary": {...}}`) carrying the
    /// derivation counts, cache copies, and per-failure
    /// `log_tail`s — all the substance the Concourse classifier
    /// used to extract inline. Falls back to the byte-elided
    /// `Value::String` when no string classifier matched.
    pub inner: serde_json::Value,
}

/// Identity-matched call-level wrapper for `concourse_get_build_logs`.
/// Owns the concourse-specific framing (timestamps, task markers,
/// final-lines defence) and delegates the bulk-content extraction to
/// the walker → `StringClassifierChain` pipeline. The chain is shared
/// with `GenericFallback` at registration time — same instance,
/// avoiding duplicate `NixStringClassifier`s across registrations.
pub struct ConcourseBuildLogClassifier {
    chain: Option<Arc<StringClassifierChain>>,
    threshold_bytes: usize,
}

impl ConcourseBuildLogClassifier {
    pub fn new(chain: Arc<StringClassifierChain>) -> Self {
        Self {
            chain: Some(chain),
            threshold_bytes: DEFAULT_GENERIC_THRESHOLD_BYTES,
        }
    }
}

impl Default for ConcourseBuildLogClassifier {
    fn default() -> Self {
        Self {
            chain: None,
            threshold_bytes: DEFAULT_GENERIC_THRESHOLD_BYTES,
        }
    }
}

impl ResultClassifier for ConcourseBuildLogClassifier {
    fn name(&self) -> &str {
        "concourse::build_log::v2"
    }

    fn matches(&self, tool_name: &str, _args: &serde_json::Value) -> bool {
        // Match the upstream MCP tool prefix; concourse's build-log endpoint
        // is the only one that produces large plain-text payloads. Other
        // concourse tools (list_jobs, list_pipelines) return small JSON
        // and fall through to GenericFallback's `Passthrough`.
        tool_name == "concourse_get_build_logs"
    }

    fn classify(&self, ctx: &ClassifierContext<'_>) -> Result<Classification, ClassifierError> {
        // `extract_text` prefers `structured_content.logs` when set —
        // exactly the bytes whose offsets the parsed summary references.
        // Returning the same `raw` as `canonical_text` is what makes
        // `tool_output_fetch(invocation_id, query: tail/range/grep)`
        // operate on the bytes the summary's slicing actually saw.
        let raw = extract_text(ctx.raw);
        let mut summary = parse_concourse_log(&raw);

        // Hand the raw log content to the walker → string classifier
        // chain. Whatever the chain produces (typed nix_build sentinel,
        // generic byte-elision, or passthrough for tiny logs) becomes
        // the `inner` field. Substance — derivation counts, failure
        // blocks with their builder log_tails — lives there now.
        summary.inner = match self.chain.as_deref() {
            Some(chain) => {
                let value = serde_json::Value::String(raw.clone());
                match walker::classify_value(&value, self.threshold_bytes, Some(chain)) {
                    WalkOutcome::Passthrough(v) => v,
                    WalkOutcome::Elided { kept, .. } => kept,
                }
            }
            None => serde_json::Value::String(raw.clone()),
        };

        Ok(Classification {
            summary: ToolResultSummary::ConcourseLogs(summary),
            canonical_text: raw,
        })
    }
}

fn extract_text(result: &rmcp::model::CallToolResult) -> String {
    // Concourse returns `{"logs": "..."}` as structured_content; the
    // text content mirrors the `logs` string. Extract whichever is set —
    // structured_content first because it skips an extra parse roundtrip.
    if let Some(serde_json::Value::Object(obj)) = result.structured_content.as_ref() {
        if let Some(serde_json::Value::String(s)) = obj.get("logs") {
            return s.clone();
        }
    }
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

/// Pure-fn entry point used by both the trait impl and the unit/integration
/// tests. Public to the crate so `core/tests/concourse_classifier.rs` can
/// drive it directly against the recorded fixtures without constructing a
/// full `CallToolResult`.
/// Extracts only the concourse-specific meta from the log: task phases,
/// warnings, stray non-nix `error:` lines, final defence-in-depth tail,
/// status. Nix-shaped substance (failure blocks, derivation counts,
/// cache copies, builder log tails) is left for the walker chain to
/// extract via `NixStringClassifier`. The classifier's `inner` field
/// holds the chain's output.
pub(crate) fn parse_concourse_log(raw: &str) -> ConcourseBuildLogSummary {
    let timestamp_re = timestamp_re();
    let task_marker_re = task_marker_re();
    let warning_re = warning_re();
    let stray_error_re = stray_error_re();
    let nix_failure_re = nix_failure_header_re();

    let mut task_phases: Vec<String> = Vec::new();
    let mut warnings: Vec<TimestampedLine> = Vec::new();
    let mut errors: Vec<TimestampedLine> = Vec::new();
    let mut nix_failure_seen = false;

    let raw_lines: Vec<&str> = raw.lines().collect();
    let total_lines = raw_lines.len() as u32;

    for line in raw_lines.iter() {
        let stripped = strip_ansi(line);
        let (timestamp, body) = split_timestamp(&stripped, timestamp_re);

        if let Some(caps) = task_marker_re.captures(body) {
            if let Some(name) = caps.get(1) {
                task_phases.push(name.as_str().trim().to_string());
            }
        } else if warning_re.is_match(body) {
            if warnings.len() < MAX_WARNINGS_KEPT {
                warnings.push(TimestampedLine {
                    timestamp: timestamp.to_string(),
                    message: body.trim().to_string(),
                });
            }
        } else if nix_failure_re.is_match(body) {
            // The walker chain's NixStringClassifier captures the
            // detail; we just need to know whether *any* nix failure
            // was seen so `status` can escalate to Failed.
            nix_failure_seen = true;
        } else if stray_error_re.is_match(body) {
            // `error: <thing>` that's NOT a structured nix failure
            // header — shell-script error, ad-hoc bash diagnostic.
            // Escalate `status` to Failed.
            if errors.len() < MAX_ERRORS_KEPT {
                errors.push(TimestampedLine {
                    timestamp: timestamp.to_string(),
                    message: body.trim().to_string(),
                });
            }
        }
    }

    let final_lines: Vec<String> = raw_lines
        .iter()
        .rev()
        .take(FINAL_TAIL_LINES)
        .rev()
        .map(|l| strip_ansi(l).into_owned())
        .collect();

    let status = if nix_failure_seen || !errors.is_empty() {
        ConcourseBuildStatus::Failed
    } else if raw_lines.is_empty() {
        ConcourseBuildStatus::Unknown
    } else {
        ConcourseBuildStatus::NoFailureDetected
    };

    let total_bytes = raw.len() as u64;

    // Rough kept_bytes estimate for the inspector's compression-ratio
    // column. The bulk of the structured shape now lives in
    // `inner` — sized after walk; this counter is just the
    // concourse-meta envelope, so kept_bytes is a lower bound on
    // the agent's view (the walker contributes the rest).
    let mut kept_bytes: usize = 0;
    for w in &warnings {
        kept_bytes += w.message.len() + w.timestamp.len();
    }
    for e in &errors {
        kept_bytes += e.message.len() + e.timestamp.len();
    }
    for l in &final_lines {
        kept_bytes += l.len();
    }
    kept_bytes += task_phases.iter().map(|s| s.len()).sum::<usize>();

    ConcourseBuildLogSummary {
        status,
        task_phases,
        warnings,
        errors,
        final_lines,
        total_lines,
        total_bytes,
        kept_bytes: kept_bytes.min(u32::MAX as usize) as u32,
        // Filled in by `ConcourseBuildLogClassifier::classify` after
        // running the walker chain. Tests that drive `parse_concourse_log`
        // directly get an empty placeholder.
        inner: serde_json::Value::Null,
    }
}

/// Concourse logs use literal ANSI escapes (e.g. `\e[1;36mINFO:\e[0m`). The
/// regexes that follow operate on stripped text so a colour code in the
/// middle of a `warning:` line doesn't hide the match. `Cow` to avoid
/// allocation when the line had no escapes.
fn strip_ansi(s: &str) -> std::borrow::Cow<'_, str> {
    static ANSI: OnceLock<Regex> = OnceLock::new();
    let re = ANSI.get_or_init(|| Regex::new(r"\x1b\[[0-9;]*m").expect("ansi regex"));
    re.replace_all(s, "")
}

/// Split off a leading `[HH:MM:SS] ` prefix when present. Returned slice
/// borrows from the input; the timestamp is empty when the line had none.
fn split_timestamp<'a>(line: &'a str, re: &Regex) -> (&'a str, &'a str) {
    if let Some(m) = re.captures(line) {
        let whole = m.get(0).expect("group 0 always present");
        let ts = m.get(1).map_or("", |g| g.as_str());
        return (ts, &line[whole.end()..]);
    }
    ("", line)
}

fn timestamp_re() -> &'static Regex {
    // Consume only the closing `] ` separator, not trailing indentation —
    // failure-continuation lines (`        > ...`) need their leading
    // whitespace preserved on the body so they stay distinguishable from
    // regular log lines.
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^\[(\d{2}:\d{2}:\d{2})\] ?").expect("timestamp regex"))
}

fn task_marker_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"=== with-nix-cache: ([^=]+?) ===").expect("task marker regex"))
}

fn warning_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^warning:").expect("warning regex"))
}

/// Stray `error:` lines. Matched only after `nix_failure_header_re`
/// has been ruled out — the parse cascade in `parse_concourse_log`
/// checks them in order, so this regex doesn't need a negative
/// lookahead (Rust's regex crate doesn't support look-around anyway).
fn stray_error_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^error:").expect("stray-error regex"))
}

/// Detects whether the log contains *any* nix failure signal (either
/// `error: failed to build attribute '...'` or `error: builder for
/// '/nix/store/...' failed`). Used only to escalate `status` to
/// Failed — the per-failure detail is captured downstream by the
/// walker chain's `NixStringClassifier`.
fn nix_failure_header_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"^error: (failed to build attribute|builder for '/nix/store/)")
            .expect("nix-failure-header regex")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_only_concourse_get_build_logs() {
        let c = ConcourseBuildLogClassifier::default();
        let args = serde_json::json!({});
        assert!(c.matches("concourse_get_build_logs", &args));
        assert!(!c.matches("concourse_list_pipelines", &args));
        assert!(!c.matches("bash", &args));
    }

    #[test]
    fn split_timestamp_pulls_prefix() {
        let re = timestamp_re();
        let (ts, body) = split_timestamp("[04:01:31] hello", re);
        assert_eq!(ts, "04:01:31");
        assert_eq!(body, "hello");
        let (ts2, body2) = split_timestamp("no prefix", re);
        assert_eq!(ts2, "");
        assert_eq!(body2, "no prefix");
    }

    #[test]
    fn ansi_strip_removes_colour_escapes() {
        let s = "\x1b[1;36mINFO:\x1b[0m text";
        assert_eq!(strip_ansi(s).as_ref(), "INFO: text");
    }

    #[test]
    fn synthetic_clean_log_has_no_failure_signal() {
        // Concourse meta only — `parse_concourse_log` no longer extracts
        // nix-shaped substance (cache copies, derivation counts, failure
        // blocks). Those land in `inner` after the walker chain runs,
        // and are exercised by the integration test
        // `core/tests/concourse_classifier.rs`.
        let raw = "[04:01:31] === with-nix-cache: start 04:01:31 ===\n\
                   [04:01:31] copying path '/nix/store/abc-foo'\n\
                   [04:01:31] copying path '/nix/store/def-bar'\n\
                   [04:01:32] checking derivation packages.x86_64-linux.default...\n\
                   [04:01:33] warning: app 'apps.x86_64-linux.bats' lacks attribute 'meta'\n\
                   [04:05:53] nix-cache: done saving to local cache\n";
        let summary = parse_concourse_log(raw);
        assert_eq!(summary.status, ConcourseBuildStatus::NoFailureDetected);
        assert_eq!(summary.warnings.len(), 1);
        assert_eq!(summary.warnings[0].timestamp, "04:01:33");
        assert!(summary.warnings[0]
            .message
            .contains("apps.x86_64-linux.bats"));
        assert!(summary.errors.is_empty());
        assert!(!summary.task_phases.is_empty());
    }

    /// Bugbot review on PR #309: shell errors / task timeouts / runner
    /// crashes leave non-empty logs without the structured nix
    /// `error: failed to build attribute` block. Such logs must not
    /// land in `NoFailureDetected` (which would mislead the agent into
    /// treating the build as healthy).
    #[test]
    fn shell_error_outside_nix_block_escalates_to_failed() {
        let raw = "[04:00:11] === with-nix-cache: start ===\n\
                   [04:00:12] + cd repo\n\
                   [04:00:12] + ./scripts/check-something.sh\n\
                   [04:00:13] error: scripts/check-something.sh: line 12: foo: command not found\n\
                   [04:00:13] task failed with exit code 127\n";
        let summary = parse_concourse_log(raw);
        assert_eq!(
            summary.status,
            ConcourseBuildStatus::Failed,
            "stray `error:` lines must escalate to Failed",
        );
        assert_eq!(summary.errors.len(), 1);
        assert_eq!(summary.errors[0].timestamp, "04:00:13");
        assert!(summary.errors[0].message.contains("command not found"));
    }

    #[test]
    fn nix_failure_header_escalates_status_to_failed() {
        // Concourse meta only checks that a nix-failure header was
        // present; the per-failure detail (drv path, reason, log_tail)
        // is captured downstream by the walker chain's
        // `NixStringClassifier`.
        let raw = "[04:00:11] error: failed to build attribute 'checks.x86_64-linux.clippy'\n\
                   [04:00:11]        Reason: builder failed with exit code 101.\n";
        let summary = parse_concourse_log(raw);
        assert_eq!(summary.status, ConcourseBuildStatus::Failed);
        // No structured failure here at the concourse layer — just the
        // status escalation.
        assert!(summary.errors.is_empty());
    }

    #[test]
    fn empty_log_is_unknown() {
        let summary = parse_concourse_log("");
        assert_eq!(summary.status, ConcourseBuildStatus::Unknown);
        assert_eq!(summary.total_lines, 0);
    }

    #[test]
    fn warnings_capped_at_max() {
        let mut raw = String::new();
        for i in 0..(MAX_WARNINGS_KEPT + 5) {
            raw.push_str(&format!("[00:00:00] warning: warn #{i}\n"));
        }
        let summary = parse_concourse_log(&raw);
        assert_eq!(summary.warnings.len(), MAX_WARNINGS_KEPT);
    }
}
