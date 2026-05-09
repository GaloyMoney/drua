//! Concourse `concourse_get_build_logs` classifier — preprocesses
//! (strip ANSI + `[HH:MM:SS]`), then delegates to the walker. Emits
//! `Typed { typed_kind: "concourse_logs", body: { logs: String } }`,
//! schema-faithful to the upstream tool's `output_schema`.

use std::sync::{Arc, OnceLock};

use regex::Regex;

use super::string_summarizer::StringSummarizerChain;
use super::walker::{self, WalkOutcome};
use super::{
    Classification, ClassifierContext, ClassifierError, ResultClassifier, ToolResultSummary,
    DEFAULT_GENERIC_THRESHOLD_BYTES,
};

pub const CONCOURSE_LOGS_KIND: &str = "concourse_logs";

/// Strips ANSI + leading `[HH:MM:SS] ` from each line. Output is
/// line-aligned with the input.
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
        "concourse::build_log::v5"
    }

    fn matches(&self, tool_name: &str, _args: &serde_json::Value) -> bool {
        tool_name == "concourse_get_build_logs"
    }

    fn classify(&self, ctx: &ClassifierContext<'_>) -> Result<Classification, ClassifierError> {
        let raw = extract_text(ctx.raw);
        let stripped = ConcourseBuildLogPreprocessor::run(&raw);

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

        let body = serde_json::json!({ "logs": logs });

        Ok(Classification {
            summary: ToolResultSummary::Typed {
                typed_kind: CONCOURSE_LOGS_KIND.to_string(),
                body,
            },
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
    fn classifier_emits_typed_summary_with_concourse_logs_kind() {
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
        let summary = ConcourseBuildLogClassifier::new(chain)
            .classify(&ClassifierContext {
                tool_name: "concourse_get_build_logs",
                args: &serde_json::json!({}),
                raw: &result,
                exit_code: None,
            })
            .expect("classify")
            .summary;

        let (typed_kind, body) = match summary {
            ToolResultSummary::Typed { typed_kind, body } => (typed_kind, body),
            other => panic!("expected Typed, got {other:?}"),
        };
        assert_eq!(typed_kind, CONCOURSE_LOGS_KIND);

        let obj = body.as_object().expect("body is an object");
        assert_eq!(obj.len(), 1);
        let logs = obj
            .get("logs")
            .and_then(|v| v.as_str())
            .expect("logs is a string");
        assert!(logs.contains("=== with-nix-cache: start ==="));
        assert!(logs.contains("<nix-copy"));
        assert!(!logs.contains("copying path"));
    }

    #[test]
    fn empty_log_yields_empty_logs() {
        let mut result = CallToolResult::success(vec![Content::text(String::new())]);
        result.structured_content = Some(serde_json::json!({"logs": ""}));
        let chain = Arc::new(crate::default_summarizer_chain());
        let summary = ConcourseBuildLogClassifier::new(chain)
            .classify(&ClassifierContext {
                tool_name: "concourse_get_build_logs",
                args: &serde_json::json!({}),
                raw: &result,
                exit_code: None,
            })
            .expect("classify")
            .summary;
        match summary {
            ToolResultSummary::Typed { body, .. } => {
                let logs = body.get("logs").and_then(|v| v.as_str()).expect("logs");
                assert_eq!(logs, "");
            }
            other => panic!("expected Typed, got {other:?}"),
        }
    }

    #[test]
    fn summary_kind_returns_typed_kind_not_serde_tag() {
        let raw = "[04:00:00] line\n";
        let mut result = CallToolResult::success(vec![Content::text(raw.to_string())]);
        result.structured_content = Some(serde_json::json!({"logs": raw}));
        let chain = Arc::new(crate::default_summarizer_chain());
        let summary = ConcourseBuildLogClassifier::new(chain)
            .classify(&ClassifierContext {
                tool_name: "concourse_get_build_logs",
                args: &serde_json::json!({}),
                raw: &result,
                exit_code: None,
            })
            .expect("classify")
            .summary;
        assert_eq!(summary.kind(), CONCOURSE_LOGS_KIND);
    }
}
