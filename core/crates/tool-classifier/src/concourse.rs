//! Concourse build-log handling, split into two pieces:
//!
//! - [`ConcourseBuildLogPreprocessor`] — strips ANSI + the leading
//!   `[HH:MM:SS] ` timestamp from each line of `logs:String`, and
//!   captures envelope metadata (first/last timestamp, warnings,
//!   errors with line numbers, task phases, final-line tail). The
//!   `logs` field stays a `String`; the schema is augmented with
//!   sibling metadata, not replaced.
//!
//! - [`ConcourseBuildLogClassifier`] — top-level [`ResultClassifier`]
//!   for `concourse_get_build_logs`. Runs the preprocessor, then
//!   hands the augmented value to the JSON walker (with the same
//!   `StringSummarizerChain` that `GenericFallback` uses) so Nix
//!   passes can collapse the bulk of `logs` into XML markers.
//!
//! Status is no longer guessed from log content. Concourse builds
//! don't emit a positive "build passed" line, and shell-script
//! errors / runner crashes / task timeouts all leave non-failure-
//! shaped logs. The summary's `status` field is `Option<…>`; the
//! preprocessor leaves it `None` and only an upstream API call (the
//! `concourse_get_build_status` tool) can fill it in.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};

use super::string_summarizer::{LogContext, StringSummarizerChain};
use super::{
    Classification, ClassifierContext, ClassifierError, ResultClassifier, ToolResultSummary,
};

const FINAL_TAIL_LINES: usize = 30;
const MAX_WARNINGS_KEPT: usize = 50;
const MAX_ERRORS_KEPT: usize = 50;

/// Build-status variants the classifier may carry. `None` is the
/// default — the log alone never proves a build succeeded, and only
/// an `error: builder for ...drv failed` block proves one failed.
/// Higher-level callers fill this in from the Concourse API when
/// the call really matters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConcourseBuildStatus {
    Succeeded,
    Failed,
    Aborted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TimestampedLine {
    /// `HH:MM:SS` extracted from the leading `[HH:MM:SS]` prefix, when
    /// present. Empty string when the line had no timestamp.
    pub timestamp: String,
    /// 1-based index of this line in the *post-preprocess* `logs`
    /// string — the same coordinate space the classifier's marker
    /// `original-lines` attributes use, so an agent that finds a
    /// warning interesting can `tool_output_fetch(range=...)`
    /// around it.
    pub line_number: u32,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConcourseBuildLogSummary {
    /// `None` when the classifier didn't have API ground truth.
    /// Higher-level call sites that pair this classifier with
    /// `concourse_get_build_status` should set it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ConcourseBuildStatus>,
    /// First `[HH:MM:SS]` seen in the raw log (before stripping).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_timestamp: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_timestamp: Option<String>,
    /// Names from the `=== with-nix-cache: <name> ===` task markers.
    pub task_phases: Vec<String>,
    pub warnings: Vec<TimestampedLine>,
    pub errors: Vec<TimestampedLine>,
    /// Last [`FINAL_TAIL_LINES`] lines verbatim — defence in depth.
    pub final_lines: Vec<String>,
    /// The compacted log itself: timestamps stripped, summarisable
    /// runs replaced with XML marker blocks (`<nix-copy>`, etc).
    pub logs: String,
    pub total_lines: u32,
    pub total_bytes: u64,
    pub kept_bytes: u32,
}

/// Preprocesses a Concourse `{logs: "<raw>"}` value. Strips ANSI +
/// timestamp prefixes from each line, computes envelope metadata,
/// returns the augmented value (still a JSON object with `logs:
/// String`) plus the metadata struct ready for the typed summary.
pub struct ConcourseBuildLogPreprocessor;

impl ConcourseBuildLogPreprocessor {
    pub fn run(raw: &str) -> ConcoursePreprocessed {
        let timestamp_re = timestamp_re();
        let task_marker_re = task_marker_re();
        let warning_re = warning_re();
        let stray_error_re = stray_error_re();

        let mut task_phases: Vec<String> = Vec::new();
        let mut warnings: Vec<TimestampedLine> = Vec::new();
        let mut errors: Vec<TimestampedLine> = Vec::new();
        let mut first_timestamp: Option<String> = None;
        let mut last_timestamp: Option<String> = None;

        let raw_lines: Vec<&str> = raw.lines().collect();
        let total_lines = raw_lines.len() as u32;
        let mut stripped = String::with_capacity(raw.len());

        for (i, line) in raw_lines.iter().enumerate() {
            let no_ansi = strip_ansi(line);
            let (timestamp, body) = split_timestamp(&no_ansi, timestamp_re);
            let line_number = (i + 1) as u32;

            if !timestamp.is_empty() {
                if first_timestamp.is_none() {
                    first_timestamp = Some(timestamp.to_string());
                }
                last_timestamp = Some(timestamp.to_string());
            }

            if let Some(caps) = task_marker_re.captures(body) {
                if let Some(name) = caps.get(1) {
                    task_phases.push(name.as_str().trim().to_string());
                }
            } else if warning_re.is_match(body) && warnings.len() < MAX_WARNINGS_KEPT {
                warnings.push(TimestampedLine {
                    timestamp: timestamp.to_string(),
                    line_number,
                    message: body.trim().to_string(),
                });
            } else if stray_error_re.is_match(body) && errors.len() < MAX_ERRORS_KEPT {
                errors.push(TimestampedLine {
                    timestamp: timestamp.to_string(),
                    line_number,
                    message: body.trim().to_string(),
                });
            }

            stripped.push_str(body);
            stripped.push('\n');
        }

        let final_lines: Vec<String> = raw_lines
            .iter()
            .rev()
            .take(FINAL_TAIL_LINES)
            .rev()
            .map(|l| {
                let no_ansi = strip_ansi(l);
                let (_, body) = split_timestamp(&no_ansi, timestamp_re);
                body.to_string()
            })
            .collect();

        ConcoursePreprocessed {
            logs: stripped,
            first_timestamp,
            last_timestamp,
            task_phases,
            warnings,
            errors,
            final_lines,
            total_lines,
            total_bytes: raw.len() as u64,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConcoursePreprocessed {
    pub logs: String,
    pub first_timestamp: Option<String>,
    pub last_timestamp: Option<String>,
    pub task_phases: Vec<String>,
    pub warnings: Vec<TimestampedLine>,
    pub errors: Vec<TimestampedLine>,
    pub final_lines: Vec<String>,
    pub total_lines: u32,
    pub total_bytes: u64,
}

/// Identity-matched call-level wrapper for `concourse_get_build_logs`.
/// Owns the Concourse-specific framing and delegates bulk-content
/// compression to the same `StringSummarizerChain`
/// `GenericFallback` uses.
#[derive(Default)]
pub struct ConcourseBuildLogClassifier {
    chain: Option<Arc<StringSummarizerChain>>,
}

impl ConcourseBuildLogClassifier {
    pub fn new(chain: Arc<StringSummarizerChain>) -> Self {
        Self { chain: Some(chain) }
    }
}

impl ResultClassifier for ConcourseBuildLogClassifier {
    fn name(&self) -> &str {
        "concourse::build_log::v3"
    }

    fn matches(&self, tool_name: &str, _args: &serde_json::Value) -> bool {
        tool_name == "concourse_get_build_logs"
    }

    fn classify(&self, ctx: &ClassifierContext<'_>) -> Result<Classification, ClassifierError> {
        let raw = extract_text(ctx.raw);
        let pre = ConcourseBuildLogPreprocessor::run(&raw);

        // Run the chain directly on the stripped logs — no JSON
        // walker, no byte-elision fallback. For Concourse the chain
        // IS the compaction strategy: if its passes can't shrink
        // the log enough, byte-eliding the result would destroy
        // the XML marker structure that's the whole point.
        let compacted = match self.chain.as_deref() {
            Some(chain) => {
                let mut log_ctx = LogContext::from_initial(&pre.logs);
                chain.run(&mut log_ctx);
                log_ctx.into_log()
            }
            None => pre.logs.clone(),
        };

        let kept_bytes = compute_kept_bytes(&pre, &compacted);

        let summary = ConcourseBuildLogSummary {
            status: None,
            first_timestamp: pre.first_timestamp,
            last_timestamp: pre.last_timestamp,
            task_phases: pre.task_phases,
            warnings: pre.warnings,
            errors: pre.errors,
            final_lines: pre.final_lines,
            logs: compacted,
            total_lines: pre.total_lines,
            total_bytes: pre.total_bytes,
            kept_bytes,
        };

        Ok(Classification {
            summary: ToolResultSummary::ConcourseLogs(summary),
            // Persist the *stripped* logs as canonical_text so
            // marker `original-lines` attributes refer to the same
            // bytes `tool_output_fetch` will return.
            canonical_text: pre.logs,
        })
    }
}

fn compute_kept_bytes(pre: &ConcoursePreprocessed, logs: &str) -> u32 {
    let mut total: usize = logs.len();
    for w in &pre.warnings {
        total = total.saturating_add(w.message.len() + w.timestamp.len() + 8);
    }
    for e in &pre.errors {
        total = total.saturating_add(e.message.len() + e.timestamp.len() + 8);
    }
    for l in &pre.final_lines {
        total = total.saturating_add(l.len() + 1);
    }
    for p in &pre.task_phases {
        total = total.saturating_add(p.len() + 4);
    }
    total.min(u32::MAX as usize) as u32
}

fn extract_text(result: &rmcp::model::CallToolResult) -> String {
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

fn strip_ansi(s: &str) -> std::borrow::Cow<'_, str> {
    static ANSI: OnceLock<Regex> = OnceLock::new();
    let re = ANSI.get_or_init(|| Regex::new(r"\x1b\[[0-9;]*m").expect("ansi regex"));
    re.replace_all(s, "")
}

fn split_timestamp<'a>(line: &'a str, re: &Regex) -> (&'a str, &'a str) {
    if let Some(m) = re.captures(line) {
        let whole = m.get(0).expect("group 0 always present");
        let ts = m.get(1).map_or("", |g| g.as_str());
        return (ts, &line[whole.end()..]);
    }
    ("", line)
}

fn timestamp_re() -> &'static Regex {
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

fn stray_error_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^error:").expect("stray-error regex"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{CallToolResult, Content};

    #[test]
    fn matches_only_concourse_get_build_logs() {
        let c = ConcourseBuildLogClassifier::default();
        let args = serde_json::json!({});
        assert!(c.matches("concourse_get_build_logs", &args));
        assert!(!c.matches("concourse_list_pipelines", &args));
        assert!(!c.matches("bash", &args));
    }

    #[test]
    fn preprocessor_strips_ansi_and_timestamps() {
        let raw = "\x1b[1;36m[04:01:31] INFO: hello\x1b[0m\n[04:01:32] world\n";
        let pre = ConcourseBuildLogPreprocessor::run(raw);
        assert_eq!(pre.logs, "INFO: hello\nworld\n");
        assert_eq!(pre.first_timestamp.as_deref(), Some("04:01:31"));
        assert_eq!(pre.last_timestamp.as_deref(), Some("04:01:32"));
    }

    #[test]
    fn preprocessor_captures_warnings_with_line_numbers() {
        let raw = "\
[04:01:31] === with-nix-cache: start ===
[04:01:32] doing things
[04:01:33] warning: app 'apps.x86_64-linux.bats' lacks attribute 'meta'
[04:01:34] more things
";
        let pre = ConcourseBuildLogPreprocessor::run(raw);
        assert_eq!(pre.warnings.len(), 1);
        let w = &pre.warnings[0];
        assert_eq!(w.timestamp, "04:01:33");
        assert_eq!(w.line_number, 3);
        assert!(w.message.contains("apps.x86_64-linux.bats"));
        assert!(!pre.task_phases.is_empty());
    }

    #[test]
    fn preprocessor_captures_stray_errors() {
        let raw = "\
[04:00:11] === with-nix-cache: start ===
[04:00:12] + cd repo
[04:00:13] error: scripts/check-something.sh: line 12: foo: command not found
[04:00:13] task failed with exit code 127
";
        let pre = ConcourseBuildLogPreprocessor::run(raw);
        assert_eq!(pre.errors.len(), 1);
        assert_eq!(pre.errors[0].timestamp, "04:00:13");
        assert_eq!(pre.errors[0].line_number, 3);
        assert!(pre.errors[0].message.contains("command not found"));
    }

    #[test]
    fn classifier_produces_typed_concourse_logs_summary() {
        let raw = "\
[04:01:31] === with-nix-cache: start ===
[04:01:32] copying path '/nix/store/aaa-foo' from 'cache'
[04:01:32] copying path '/nix/store/bbb-bar' from 'cache'
[04:01:32] copying path '/nix/store/ccc-baz' from 'cache'
[04:01:33] all done
";
        let mut result = CallToolResult::success(vec![Content::text(raw.to_string())]);
        result.structured_content = Some(serde_json::json!({"logs": raw}));
        let args = serde_json::json!({"build_id": 1u64});
        let chain = Arc::new(crate::default_summarizer_chain());
        let cls = ConcourseBuildLogClassifier::new(chain);
        let classification = cls
            .classify(&ClassifierContext {
                tool_name: "concourse_get_build_logs",
                args: &args,
                raw: &result,
                exit_code: None,
            })
            .expect("classify");
        let summary = match classification.summary {
            ToolResultSummary::ConcourseLogs(s) => s,
            other => panic!("expected ConcourseLogs, got {other:?}"),
        };
        assert!(summary.status.is_none());
        assert!(summary.logs.contains("=== with-nix-cache: start ==="));
        assert!(summary.logs.contains("<nix-copy"));
        assert!(summary.logs.contains("</nix-copy>"));
        assert!(!summary.logs.contains("copying path"));
        assert_eq!(summary.first_timestamp.as_deref(), Some("04:01:31"));
    }

    #[test]
    fn empty_log_yields_zero_total_lines() {
        let pre = ConcourseBuildLogPreprocessor::run("");
        assert_eq!(pre.total_lines, 0);
        assert!(pre.first_timestamp.is_none());
        assert!(pre.warnings.is_empty());
    }

    #[test]
    fn warnings_capped_at_max() {
        let mut raw = String::new();
        for i in 0..(MAX_WARNINGS_KEPT + 5) {
            raw.push_str(&format!("[00:00:00] warning: warn #{i}\n"));
        }
        let pre = ConcourseBuildLogPreprocessor::run(&raw);
        assert_eq!(pre.warnings.len(), MAX_WARNINGS_KEPT);
    }
}
