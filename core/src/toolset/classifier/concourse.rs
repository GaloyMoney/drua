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
use std::sync::OnceLock;

use super::{
    Classification, ClassifierContext, ClassifierError, ResultClassifier, ToolResultSummary,
};

/// Lines kept from the very end of the log unconditionally. Bounded so a
/// pathological tail of cache-pruning still doesn't blow context, but big
/// enough to almost always include any final summary line.
const FINAL_TAIL_LINES: usize = 30;

/// Per-failure cap on captured `> ...` log lines. Concourse / nix already
/// truncate to "Last 25 log lines" by convention, so 40 leaves headroom
/// without re-introducing unbounded growth.
const MAX_FAILURE_LOG_TAIL: usize = 40;

/// Defence: a deeply-pathological log shouldn't allocate a million
/// warnings / errors into the summary.
const MAX_WARNINGS_KEPT: usize = 50;
const MAX_ERRORS_KEPT: usize = 50;
const MAX_FAILURES_KEPT: usize = 10;

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
pub struct NixBuildFailure {
    /// e.g. `"checks.x86_64-linux.clippy"`.
    pub attribute: String,
    /// The `/nix/store/...drv` path mentioned in the failure header,
    /// when extractable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drv: Option<String>,
    /// "builder failed with exit code 101" or similar one-liner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The indented `> ...` lines emitted as nix's own "Last N log lines"
    /// trailer — the actual rust/clippy/test diagnostic.
    pub log_tail: Vec<String>,
    /// `[HH:MM:SS]` of the failure header, useful for correlating across
    /// parallel derivations.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConcourseBuildLogSummary {
    pub status: ConcourseBuildStatus,
    /// Names from the `=== with-nix-cache: <name> ===` task markers.
    pub task_phases: Vec<String>,
    pub nix_paths_copied: u32,
    pub nix_paths_built: u32,
    pub derivations_checked: u32,
    pub cache_files_pruned: u32,
    pub warnings: Vec<TimestampedLine>,
    /// Stray `error:` lines that didn't open a structured nix failure
    /// block — shell-script errors, ad-hoc bash scripts emitting
    /// `error: foo`, build-task wrapper diagnostics. Each such line
    /// also escalates `status` to `Failed`.
    pub errors: Vec<TimestampedLine>,
    /// Structured nix `error: failed to build attribute '...'` blocks,
    /// each with the captured "Last N log lines" trailer.
    pub failures: Vec<NixBuildFailure>,
    /// Always-on tail — last [`FINAL_TAIL_LINES`] lines verbatim.
    pub final_lines: Vec<String>,
    pub total_lines: u32,
    pub total_bytes: u64,
    /// Approximate number of bytes the agent now sees vs the raw input.
    /// `total_bytes / kept_bytes` is the compression ratio the inspector
    /// renders.
    pub kept_bytes: u32,
}

pub struct ConcourseBuildLogClassifier;

impl ResultClassifier for ConcourseBuildLogClassifier {
    fn name(&self) -> &str {
        "concourse::build_log::v1"
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
        let summary = parse_concourse_log(&raw);
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
pub(crate) fn parse_concourse_log(raw: &str) -> ConcourseBuildLogSummary {
    let timestamp_re = timestamp_re();
    let task_marker_re = task_marker_re();
    let copying_re = copying_path_re();
    let building_re = building_drv_re();
    let checking_re = checking_derivation_re();
    let pruning_re = pruning_re();
    let warning_re = warning_re();
    let failure_re = failure_header_re();
    let reason_re = reason_re();
    let log_line_re = log_line_re();
    let stray_error_re = stray_error_re();

    let mut nix_paths_copied = 0u32;
    let mut nix_paths_built = 0u32;
    let mut derivations_checked = 0u32;
    let mut cache_files_pruned = 0u32;
    let mut task_phases: Vec<String> = Vec::new();
    let mut warnings: Vec<TimestampedLine> = Vec::new();
    let mut errors: Vec<TimestampedLine> = Vec::new();
    let mut failures: Vec<NixBuildFailure> = Vec::new();

    let raw_lines: Vec<&str> = raw.lines().collect();
    let total_lines = raw_lines.len() as u32;

    // Iterate with a lookahead — failure-tail lines are consumed greedily
    // when we land on a failure header.
    let mut i = 0usize;
    while i < raw_lines.len() {
        let line = raw_lines[i];
        let stripped = strip_ansi(line);
        let (timestamp, body) = split_timestamp(&stripped, timestamp_re);

        if let Some(caps) = task_marker_re.captures(body) {
            if let Some(name) = caps.get(1) {
                task_phases.push(name.as_str().trim().to_string());
            }
        } else if copying_re.is_match(body) {
            nix_paths_copied += 1;
        } else if building_re.is_match(body) {
            nix_paths_built += 1;
        } else if checking_re.is_match(body) {
            derivations_checked += 1;
        } else if pruning_re.is_match(body) {
            cache_files_pruned += 1;
        } else if warning_re.is_match(body) {
            if warnings.len() < MAX_WARNINGS_KEPT {
                warnings.push(TimestampedLine {
                    timestamp: timestamp.to_string(),
                    message: body.trim().to_string(),
                });
            }
        } else if let Some(caps) = failure_re.captures(body) {
            // Header consumed; greedily eat the indented continuation
            // lines that nix emits as the failure block:
            //   Reason: ...
            //   Output paths: ...
            //   Last N log lines:
            //   > <rust diagnostic>
            //   > ...
            //   For full logs, run: ...
            let attribute = caps.get(1).map_or("", |m| m.as_str()).to_string();
            let drv = caps.get(2).map(|m| m.as_str().to_string());

            let mut reason: Option<String> = None;
            let mut log_tail: Vec<String> = Vec::new();
            let mut j = i + 1;
            while j < raw_lines.len() {
                let ahead = strip_ansi(raw_lines[j]);
                let (_, ahead_body) = split_timestamp(&ahead, timestamp_re);
                let trimmed = ahead_body.trim_start();

                if let Some(rcaps) = reason_re.captures(trimmed) {
                    reason = Some(rcaps.get(1).map_or("", |m| m.as_str()).trim().to_string());
                    j += 1;
                    continue;
                }
                if let Some(lcaps) = log_line_re.captures(trimmed) {
                    if log_tail.len() < MAX_FAILURE_LOG_TAIL {
                        log_tail.push(lcaps.get(1).map_or("", |m| m.as_str()).to_string());
                    }
                    j += 1;
                    continue;
                }
                // Continuation lines like "Output paths:" and "Last 25 log
                // lines:" use the same indent — keep skipping while the
                // line is indented and didn't match any of the structural
                // regexes.
                if !ahead_body.is_empty() && ahead_body.starts_with(' ') {
                    j += 1;
                    continue;
                }
                break;
            }

            if failures.len() < MAX_FAILURES_KEPT {
                failures.push(NixBuildFailure {
                    attribute,
                    drv,
                    reason,
                    log_tail,
                    timestamp: timestamp.to_string(),
                });
            }
            i = j;
            continue;
        } else if stray_error_re.is_match(body) {
            // `error: <thing>` that didn't open a structured nix failure
            // block — shell-script error, ad-hoc bash diagnostic, etc.
            // These escalate `status` to Failed even without a captured
            // log_tail.
            if errors.len() < MAX_ERRORS_KEPT {
                errors.push(TimestampedLine {
                    timestamp: timestamp.to_string(),
                    message: body.trim().to_string(),
                });
            }
        }

        i += 1;
    }

    let final_lines: Vec<String> = raw_lines
        .iter()
        .rev()
        .take(FINAL_TAIL_LINES)
        .rev()
        .map(|l| strip_ansi(l).into_owned())
        .collect();

    let status = if !failures.is_empty() || !errors.is_empty() {
        ConcourseBuildStatus::Failed
    } else if raw_lines.is_empty() {
        ConcourseBuildStatus::Unknown
    } else {
        ConcourseBuildStatus::NoFailureDetected
    };

    let total_bytes = raw.len() as u64;

    // Rough kept_bytes estimate — sum of structured payload sizes. Useful
    // for the inspector's compression-ratio column without re-serializing.
    let mut kept_bytes: usize = 0;
    for w in &warnings {
        kept_bytes += w.message.len() + w.timestamp.len();
    }
    for e in &errors {
        kept_bytes += e.message.len() + e.timestamp.len();
    }
    for f in &failures {
        kept_bytes += f.attribute.len();
        kept_bytes += f.drv.as_ref().map_or(0, |s| s.len());
        kept_bytes += f.reason.as_ref().map_or(0, |s| s.len());
        kept_bytes += f.timestamp.len();
        for l in &f.log_tail {
            kept_bytes += l.len();
        }
    }
    for l in &final_lines {
        kept_bytes += l.len();
    }
    kept_bytes += task_phases.iter().map(|s| s.len()).sum::<usize>();

    ConcourseBuildLogSummary {
        status,
        task_phases,
        nix_paths_copied,
        nix_paths_built,
        derivations_checked,
        cache_files_pruned,
        warnings,
        errors,
        failures,
        final_lines,
        total_lines,
        total_bytes,
        kept_bytes: kept_bytes.min(u32::MAX as usize) as u32,
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

fn copying_path_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^copying path '/nix/store/").expect("copying-path regex"))
}

fn building_drv_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^building '/nix/store/[^']+\.drv'").expect("building-drv regex"))
}

fn checking_derivation_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"^(checking derivation|derivation evaluated to)").expect("checking-drv regex")
    })
}

fn pruning_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^nix-cache: removing ").expect("pruning regex"))
}

fn warning_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^warning:").expect("warning regex"))
}

/// Stray `error:` lines — anything that opens with `error:` but didn't
/// match the structured nix-failure header. The exclusion of `failed to
/// build attribute` is implicit: that branch is consumed earlier in the
/// match cascade, so this only sees what falls through (shell errors,
/// resource step diagnostics, ad-hoc bash `error: foo` lines).
fn stray_error_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^error:").expect("stray-error regex"))
}

fn failure_header_re() -> &'static Regex {
    // `error: failed to build attribute 'X', build of '/nix/store/...drv^*' failed: ...`
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"^error: failed to build attribute '([^']+)'(?:, build of '([^']+)' failed)?")
            .expect("failure-header regex")
    })
}

fn reason_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^Reason: (.+)$").expect("reason regex"))
}

fn log_line_re() -> &'static Regex {
    // Indented `> <rust line>` — the actual diagnostic preserved verbatim.
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^> (.*)$").expect("log-line regex"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_only_concourse_get_build_logs() {
        let c = ConcourseBuildLogClassifier;
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
        let raw = "[04:01:31] === with-nix-cache: start 04:01:31 ===\n\
                   [04:01:31] copying path '/nix/store/abc-foo'\n\
                   [04:01:31] copying path '/nix/store/def-bar'\n\
                   [04:01:32] checking derivation packages.x86_64-linux.default...\n\
                   [04:01:33] warning: app 'apps.x86_64-linux.bats' lacks attribute 'meta'\n\
                   [04:05:53] nix-cache: done saving to local cache\n";
        let summary = parse_concourse_log(raw);
        assert_eq!(summary.status, ConcourseBuildStatus::NoFailureDetected);
        assert_eq!(summary.nix_paths_copied, 2);
        assert_eq!(summary.derivations_checked, 1);
        assert_eq!(summary.warnings.len(), 1);
        assert_eq!(summary.warnings[0].timestamp, "04:01:33");
        assert!(summary.warnings[0]
            .message
            .contains("apps.x86_64-linux.bats"));
        assert!(summary.failures.is_empty());
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
        assert!(summary.failures.is_empty(), "no nix block in this fixture");
        assert_eq!(summary.errors.len(), 1);
        assert_eq!(summary.errors[0].timestamp, "04:00:13");
        assert!(summary.errors[0].message.contains("command not found"));
    }

    #[test]
    fn synthetic_failure_block_is_extracted() {
        let raw = "[04:00:11] error: failed to build attribute 'checks.x86_64-linux.clippy', build of '/nix/store/abc-clippy.drv^*' failed: Cannot build it.\n\
                   [04:00:11]        Reason: builder failed with exit code 101.\n\
                   [04:00:11]        Output paths:\n\
                   [04:00:11]          /nix/store/xyz-clippy-out\n\
                   [04:00:11]        Last 25 log lines:\n\
                   [04:00:11]        > error[E0063]: missing fields\n\
                   [04:00:11]        >    --> core/tests/toolset.rs:15\n\
                   [04:00:11]        For full logs, run:\n\
                   [04:00:11]          nix log /nix/store/abc-clippy.drv\n";
        let summary = parse_concourse_log(raw);
        assert_eq!(summary.status, ConcourseBuildStatus::Failed);
        assert_eq!(summary.failures.len(), 1);
        let f = &summary.failures[0];
        assert_eq!(f.attribute, "checks.x86_64-linux.clippy");
        assert_eq!(f.drv.as_deref(), Some("/nix/store/abc-clippy.drv^*"));
        assert_eq!(
            f.reason.as_deref(),
            Some("builder failed with exit code 101.")
        );
        assert_eq!(f.log_tail.len(), 2);
        assert!(f.log_tail[0].contains("missing fields"));
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
