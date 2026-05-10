//! Bash result classifier; emits `Typed { typed_kind: "bash" }`.

use std::sync::Arc;

use super::string_summarizer::StringSummarizerChain;
use super::walker::{self, WalkOutcome};
use super::{
    Classification, ClassifierContext, ClassifierError, ResultClassifier, ToolResultSummary,
    DEFAULT_GENERIC_THRESHOLD_BYTES,
};

pub const BASH_KIND: &str = "bash";

#[derive(Default)]
pub struct BashClassifier {
    chain: Option<Arc<StringSummarizerChain>>,
}

impl BashClassifier {
    pub fn new(chain: Arc<StringSummarizerChain>) -> Self {
        Self { chain: Some(chain) }
    }
}

impl ResultClassifier for BashClassifier {
    fn name(&self) -> &str {
        "bash::v1"
    }

    fn matches(&self, tool_name: &str, _args: &serde_json::Value) -> bool {
        tool_name == "bash"
    }

    fn classify(&self, ctx: &ClassifierContext<'_>) -> Result<Classification, ClassifierError> {
        let (stdout, stderr, exit_code, duration_ms) =
            parse_bash_body(ctx.raw).ok_or(ClassifierError::Failed {
                classifier: "bash::v1",
                message: "missing structured_content {stdout, stderr, exit_code, duration_ms}"
                    .to_string(),
            })?;

        let stdout_compacted = compact(&stdout, self.chain.as_deref());
        let stderr_compacted = compact(&stderr, self.chain.as_deref());

        let body = serde_json::json!({
            "stdout": stdout_compacted,
            "stderr": stderr_compacted,
            "exit_code": exit_code,
            "duration_ms": duration_ms,
        });

        let canonical_text = if stderr.is_empty() {
            stdout.clone()
        } else if stdout.is_empty() {
            stderr.clone()
        } else {
            format!("--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}")
        };

        Ok(Classification {
            summary: ToolResultSummary::Typed {
                typed_kind: BASH_KIND.to_string(),
                body,
            },
            canonical_text,
            // Bash classifier produces a record-shaped body, so the
            // transport-level wrapping is a no-op.
            root_path: "$".to_string(),
        })
    }
}

pub(crate) fn render_bash_envelope(
    body: &serde_json::Value,
    invocation_id_str: &str,
    raw_size_bytes: u64,
) -> String {
    let stdout = body.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
    let stderr = body.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
    let exit_code = body.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(0);
    let duration_ms = body
        .get("duration_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let dur = if duration_ms >= 60_000 {
        format!(
            "{}m{}s",
            duration_ms / 60_000,
            (duration_ms % 60_000) / 1000
        )
    } else if duration_ms >= 1000 {
        format!("{:.2}s", duration_ms as f64 / 1000.0)
    } else {
        format!("{duration_ms}ms")
    };
    let mut out = format!(
        "[bash exit={exit_code} in {dur}; stdout={}B stderr={}B; raw={raw_size_bytes}B; \
         fetch raw via tool_output_fetch(invocation_id=\"{invocation_id_str}\")]\n",
        stdout.len(),
        stderr.len(),
    );
    if !stdout.is_empty() {
        out.push_str("=== stdout ===\n");
        out.push_str(stdout);
        if !stdout.ends_with('\n') {
            out.push('\n');
        }
    }
    if !stderr.is_empty() {
        out.push_str("=== stderr ===\n");
        out.push_str(stderr);
        if !stderr.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

fn parse_bash_body(result: &rmcp::model::CallToolResult) -> Option<(String, String, i32, u64)> {
    let sc = result.structured_content.as_ref()?;
    let obj = sc.as_object()?;
    let stdout = obj.get("stdout")?.as_str()?.to_string();
    let stderr = obj.get("stderr")?.as_str()?.to_string();
    let exit_code = obj.get("exit_code")?.as_i64()? as i32;
    let duration_ms = obj.get("duration_ms")?.as_u64()?;
    Some((stdout, stderr, exit_code, duration_ms))
}

fn compact(stream: &str, chain: Option<&StringSummarizerChain>) -> String {
    if stream.is_empty() {
        return String::new();
    }
    let value = serde_json::Value::String(stream.to_string());
    let outcome = walker::classify_value(&value, DEFAULT_GENERIC_THRESHOLD_BYTES, chain);
    match outcome {
        WalkOutcome::Passthrough(v) => v.as_str().unwrap_or(stream).to_string(),
        WalkOutcome::Elided { kept, .. } => kept.as_str().unwrap_or(stream).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{CallToolResult, Content};

    fn raw_with(stdout: &str, stderr: &str, exit_code: i32, duration_ms: u64) -> CallToolResult {
        let mut r = CallToolResult::success(vec![Content::text("ignored")]);
        r.structured_content = Some(serde_json::json!({
            "stdout": stdout,
            "stderr": stderr,
            "exit_code": exit_code,
            "duration_ms": duration_ms,
        }));
        r
    }

    #[test]
    fn matches_only_bash() {
        let c = BashClassifier::default();
        let args = serde_json::json!({});
        assert!(c.matches("bash", &args));
        assert!(!c.matches("Read", &args));
    }

    #[test]
    fn body_carries_streams_and_exit_code() {
        let raw = raw_with("hello\n", "", 0, 12);
        let chain = Arc::new(crate::default_summarizer_chain());
        let summary = BashClassifier::new(chain)
            .classify(&ClassifierContext {
                tool_name: "bash",
                args: &serde_json::json!({}),
                raw: &raw,
                exit_code: None,
            })
            .expect("classify")
            .summary;
        match summary {
            ToolResultSummary::Typed { typed_kind, body } => {
                assert_eq!(typed_kind, BASH_KIND);
                assert_eq!(body["stdout"], "hello\n");
                assert_eq!(body["stderr"], "");
                assert_eq!(body["exit_code"], 0);
                assert_eq!(body["duration_ms"], 12);
            }
            other => panic!("expected Typed, got {other:?}"),
        }
    }

    #[test]
    fn cargo_build_stderr_compacts_via_chain() {
        let mut stderr = String::from("    Updating crates.io index\n");
        for i in 0..50 {
            stderr.push_str(&format!("Downloaded crate-{i:03} v0.1.0\n"));
        }
        for i in 0..40 {
            stderr.push_str(&format!("Compiling crate-{i:03} v0.1.0\n"));
        }
        stderr.push_str("    Finished `dev` profile [unoptimized] target(s) in 3m 33s\n");
        let raw = raw_with("", &stderr, 0, 213_000);
        let chain = Arc::new(crate::default_summarizer_chain());
        let body = match BashClassifier::new(chain)
            .classify(&ClassifierContext {
                tool_name: "bash",
                args: &serde_json::json!({}),
                raw: &raw,
                exit_code: None,
            })
            .expect("classify")
            .summary
        {
            ToolResultSummary::Typed { body, .. } => body,
            other => panic!("expected Typed, got {other:?}"),
        };
        let stderr_kept = body["stderr"].as_str().unwrap();
        assert!(stderr_kept.contains("<cargo-downloads"));
        assert!(stderr_kept.contains("<cargo-compiling"));
        assert!(stderr_kept.contains("Finished `dev`"));
        assert!(!stderr_kept.contains("Downloaded crate-001"));
    }
}
