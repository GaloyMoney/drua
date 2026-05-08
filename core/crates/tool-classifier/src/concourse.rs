//! Concourse build-log handling, split into two pieces:
//!
//! - [`ConcourseBuildLogPreprocessor`] — strips ANSI + the leading
//!   `[HH:MM:SS] ` timestamp from each line so the summarizer chain
//!   sees clean line bodies. Returns a plain `String` — no envelope
//!   metadata, no schema augmentation.
//!
//! - [`ConcourseBuildLogClassifier`] — top-level [`ResultClassifier`]
//!   for `concourse_get_build_logs`. Runs the preprocessor, hands
//!   the stripped log to a [`StringSummarizerChain`] (Nix passes,
//!   git-clone, bulk-elide), and wraps the result in a
//!   [`ConcourseBuildLogSummary`] whose shape is exactly
//!   `{ logs: String }` — schema-faithful to the upstream tool.
//!
//! Status, warnings, errors, and other "envelope" signals are no
//! longer extracted into typed sidecar fields. They survive as
//! ordinary lines inside `logs` for the agent to grep.

use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};

use regex::Regex;

use super::string_summarizer::StringSummarizerChain;
use super::walker::{self, WalkOutcome};
use super::{
    Classification, ClassifierContext, ClassifierError, ResultClassifier, ToolResultSummary,
    DEFAULT_GENERIC_THRESHOLD_BYTES,
};

/// Build-status variants. Set only when an upstream API call gives
/// ground truth — never guessed from log content. Kept as a public
/// type for higher-level callers that want to thread it through;
/// it does not appear on [`ConcourseBuildLogSummary`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConcourseBuildStatus {
    Succeeded,
    Failed,
    Aborted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TimestampedLine {
    pub timestamp: String,
    pub line_number: u32,
    pub message: String,
}

/// Schema-faithful summary: matches the upstream
/// `concourse_get_build_logs` tool's declared output_schema
/// (`{ logs: String }`). The compacted log carries every signal
/// inline — warnings, errors, and structured-pass markers all live
/// inside `logs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConcourseBuildLogSummary {
    pub logs: String,
}

/// Strips ANSI + leading `[HH:MM:SS] ` from each line of a raw
/// Concourse log. Output is line-aligned with the input — every
/// input line maps to one output line — so summarizer-pass
/// `original-lines` attributes refer to consistent coordinates.
pub struct ConcourseBuildLogPreprocessor;

impl ConcourseBuildLogPreprocessor {
    pub fn run(raw: &str) -> String {
        let timestamp_re = timestamp_re();
        let mut out = String::with_capacity(raw.len());
        for line in raw.lines() {
            let no_ansi = strip_ansi(line);
            let body = strip_timestamp(&no_ansi, timestamp_re);
            out.push_str(body);
            out.push('\n');
        }
        out
    }
}

/// Identity-matched call-level wrapper for `concourse_get_build_logs`.
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
        "concourse::build_log::v4"
    }

    fn matches(&self, tool_name: &str, _args: &serde_json::Value) -> bool {
        tool_name == "concourse_get_build_logs"
    }

    fn classify(&self, ctx: &ClassifierContext<'_>) -> Result<Classification, ClassifierError> {
        let raw = extract_text(ctx.raw);
        let stripped = ConcourseBuildLogPreprocessor::run(&raw);

        // Concourse's only unique work is preprocessing. Hand the
        // stripped log to the walker — it descends into
        // `Value::String`, runs the same chain `GenericFallback`
        // uses at every other string leaf, and returns a
        // schema-faithful Value::String back.
        let value = serde_json::Value::String(stripped.clone());
        let kept_value = match walker::classify_value(
            &value,
            DEFAULT_GENERIC_THRESHOLD_BYTES,
            self.chain.as_deref(),
        ) {
            WalkOutcome::Passthrough(v) => v,
            WalkOutcome::Elided { kept, .. } => kept,
        };
        let logs = kept_value.as_str().unwrap_or(&stripped).to_string();

        let summary = ConcourseBuildLogSummary { logs };

        Ok(Classification {
            summary: ToolResultSummary::ConcourseLogs(summary),
            // canonical_text is the bytes `tool_output_fetch`
            // operates on; stripped log so marker `original-lines`
            // attributes line up with the persisted bytes.
            canonical_text: stripped,
        })
    }
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

fn strip_timestamp<'a>(line: &'a str, re: &Regex) -> &'a str {
    if let Some(m) = re.find(line) {
        &line[m.end()..]
    } else {
        line
    }
}

fn timestamp_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^\[\d{2}:\d{2}:\d{2}\] ?").expect("timestamp regex"))
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
        let out = ConcourseBuildLogPreprocessor::run(raw);
        assert_eq!(out, "INFO: hello\nworld\n");
    }

    #[test]
    fn preprocessor_preserves_line_count() {
        let raw = "[04:01:31] line a\n[04:01:32] line b\nline c without ts\n";
        let out = ConcourseBuildLogPreprocessor::run(raw);
        assert_eq!(out.lines().count(), 3);
    }

    #[test]
    fn classifier_summary_serialises_to_only_logs_field() {
        let raw = "\
[04:01:31] === with-nix-cache: start ===
[04:01:32] copying path '/nix/store/aaa-foo' from 'cache'
[04:01:32] copying path '/nix/store/bbb-bar' from 'cache'
[04:01:32] copying path '/nix/store/ccc-baz' from 'cache'
[04:01:33] all done
";
        let mut result = CallToolResult::success(vec![Content::text(raw.to_string())]);
        result.structured_content = Some(serde_json::json!({"logs": raw}));
        let chain = Arc::new(crate::default_summarizer_chain());
        let summary = match ConcourseBuildLogClassifier::new(chain)
            .classify(&ClassifierContext {
                tool_name: "concourse_get_build_logs",
                args: &serde_json::json!({}),
                raw: &result,
                exit_code: None,
            })
            .expect("classify")
            .summary
        {
            ToolResultSummary::ConcourseLogs(s) => s,
            other => panic!("unexpected: {other:?}"),
        };
        let v = serde_json::to_value(&summary).expect("serialise");
        let obj = v.as_object().expect("summary serialises as object");
        assert_eq!(obj.len(), 1, "only `logs` field; got: {obj:?}");
        assert!(obj.contains_key("logs"));
        assert!(summary.logs.contains("=== with-nix-cache: start ==="));
        assert!(summary.logs.contains("<nix-copy"));
        assert!(!summary.logs.contains("copying path"));
    }

    #[test]
    fn empty_log_yields_empty_logs() {
        let mut result = CallToolResult::success(vec![Content::text(String::new())]);
        result.structured_content = Some(serde_json::json!({"logs": ""}));
        let chain = Arc::new(crate::default_summarizer_chain());
        let summary = match ConcourseBuildLogClassifier::new(chain)
            .classify(&ClassifierContext {
                tool_name: "concourse_get_build_logs",
                args: &serde_json::json!({}),
                raw: &result,
                exit_code: None,
            })
            .expect("classify")
            .summary
        {
            ToolResultSummary::ConcourseLogs(s) => s,
            other => panic!("unexpected: {other:?}"),
        };
        assert_eq!(summary.logs, "");
    }
}
