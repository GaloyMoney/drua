//! Tool-result classifiers and the universal `GenericFallback`.

use rmcp::model::CallToolResult;
use serde::{Deserialize, Serialize};

mod bash;
mod cargo;
mod concourse;
mod git;
mod nix;
mod rsync;
mod string_summarizer;
mod terraform_install;
mod terraform_state;
mod walker;

pub use bash::BashClassifier;
pub use cargo::{CargoCheckRun, CargoCompileRun, CargoDownloadRun};
pub use concourse::{ConcourseBuildLogClassifier, ConcourseBuildLogPreprocessor};
pub use git::GitCloneProgress;
pub use nix::{
    NixBuildingRun, NixCacheActivity, NixCopyRun, NixDerivationPreprocessor, NixDrvList,
    NixFetchList, NixImageLayerRun,
};
pub use rsync::RsyncFileList;
pub use string_summarizer::{
    build_marker, close_tag, open_tag, BulkElide, SegmentedText, StringSummarizer,
    StringSummarizerChain, VerbatimRegion,
};
pub use terraform_install::TerraformInstallRun;
pub use terraform_state::{TerraformDataSourceRun, TerraformRefreshRun};
pub use walker::RECOVERY_INVOCATION_PLACEHOLDER;

pub const DEFAULT_GENERIC_THRESHOLD_BYTES: usize = 8192;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolResultSummary {
    Passthrough {
        value: serde_json::Value,
    },

    StructuredElision {
        kept: serde_json::Value,
        elided_paths: Vec<ElidedPath>,
        total_bytes: u64,
        kept_bytes: u32,
    },

    Typed {
        typed_kind: String,
        body: serde_json::Value,
    },
}

impl ToolResultSummary {
    pub fn kind(&self) -> &str {
        match self {
            Self::Passthrough { .. } => "passthrough",
            Self::StructuredElision { .. } => "structured_elision",
            Self::Typed { typed_kind, .. } => typed_kind,
        }
    }

    pub fn is_passthrough(&self) -> bool {
        matches!(self, Self::Passthrough { .. })
    }

    pub fn kept_bytes(&self, raw_bytes: u64) -> u64 {
        match self {
            Self::Passthrough { .. } => raw_bytes,
            Self::StructuredElision { kept_bytes, .. } => *kept_bytes as u64,
            Self::Typed { typed_kind, body } => {
                if typed_kind == bash::BASH_KIND {
                    let stdout_len = body
                        .get("stdout")
                        .and_then(|v| v.as_str())
                        .map(|s| s.len() as u64)
                        .unwrap_or(0);
                    let stderr_len = body
                        .get("stderr")
                        .and_then(|v| v.as_str())
                        .map(|s| s.len() as u64)
                        .unwrap_or(0);
                    stdout_len + stderr_len
                } else {
                    canonical_text_for(body).len() as u64
                }
            }
        }
    }

    pub fn render_envelope_text(&self, invocation_id_str: &str, raw_size_bytes: u64) -> String {
        match self {
            Self::Passthrough { value } => match value {
                serde_json::Value::String(s) => s.clone(),
                other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
            },
            Self::StructuredElision {
                kept,
                elided_paths,
                total_bytes,
                kept_bytes,
            } => {
                let mut out = String::new();
                out.push_str(&format!(
                    "[json elided: {kept_bytes}/{total_bytes} bytes kept; \
                     fetch raw via tool_output_fetch(invocation_id=\"{invocation_id_str}\")]\n",
                ));
                if !elided_paths.is_empty() {
                    out.push_str("=== elided paths ===\n");
                    for p in elided_paths {
                        let kind_str = match p.kind {
                            ElisionKind::String => "string",
                            ElisionKind::Array => "array",
                            ElisionKind::Object => "object",
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
                out.push_str(&canonical_text_for(kept));
                out
            }
            Self::Typed { typed_kind, body } => {
                if typed_kind == bash::BASH_KIND {
                    bash::render_bash_envelope(body, invocation_id_str, raw_size_bytes)
                } else {
                    let mut out = format!(
                        "[{typed_kind}: {raw_size_bytes} original bytes; \
                         fetch raw via tool_output_fetch(invocation_id=\"{invocation_id_str}\")]\n",
                    );
                    out.push_str(&canonical_text_for(body));
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                    out
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElidedPath {
    pub path: String,
    pub kind: ElisionKind,
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ElisionKind {
    String,
    Array,
    Object,
}

pub struct ClassifierContext<'a> {
    pub tool_name: &'a str,
    pub args: &'a serde_json::Value,
    pub raw: &'a CallToolResult,
    pub exit_code: Option<i32>,
}

#[derive(Debug, thiserror::Error)]
pub enum ClassifierError {
    #[error("classifier {classifier} failed: {message}")]
    Failed {
        classifier: &'static str,
        message: String,
    },
}

#[derive(Debug, Clone)]
pub struct Classification {
    pub summary: ToolResultSummary,
    pub canonical_text: String,
}

pub trait ResultClassifier: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn matches(&self, tool_name: &str, args: &serde_json::Value) -> bool;
    fn classify(&self, ctx: &ClassifierContext<'_>) -> Result<Classification, ClassifierError>;
}

pub struct ClassifierRegistry {
    classifiers: Vec<Box<dyn ResultClassifier>>,
}

impl ClassifierRegistry {
    pub fn new() -> Self {
        Self {
            classifiers: Vec::new(),
        }
    }

    pub fn with_default() -> Self {
        let chain = std::sync::Arc::new(default_summarizer_chain());
        Self::new()
            .register(ConcourseBuildLogClassifier::new(std::sync::Arc::clone(
                &chain,
            )))
            .register(BashClassifier::new(std::sync::Arc::clone(&chain)))
            .register(GenericFallback::default().with_summarizer_chain(chain))
    }

    pub fn register(mut self, classifier: impl ResultClassifier) -> Self {
        self.classifiers.push(Box::new(classifier));
        self
    }

    pub fn classify(&self, ctx: &ClassifierContext<'_>) -> Classification {
        for classifier in &self.classifiers {
            if !classifier.matches(ctx.tool_name, ctx.args) {
                continue;
            }
            match classifier.classify(ctx) {
                Ok(c) => return c,
                Err(e) => {
                    tracing::warn!(
                        classifier = classifier.name(),
                        error = %e,
                        "classifier failed; trying next",
                    );
                }
            }
        }
        let canonical_text = extract_text(ctx.raw);
        Classification {
            summary: ToolResultSummary::Passthrough {
                value: serde_json::Value::String(canonical_text.clone()),
            },
            canonical_text,
        }
    }
}

impl Default for ClassifierRegistry {
    fn default() -> Self {
        Self::with_default()
    }
}

pub struct GenericFallback {
    pub threshold_bytes: usize,
    pub summarizer_chain: Option<std::sync::Arc<StringSummarizerChain>>,
}

impl Default for GenericFallback {
    fn default() -> Self {
        Self {
            threshold_bytes: DEFAULT_GENERIC_THRESHOLD_BYTES,
            summarizer_chain: None,
        }
    }
}

impl GenericFallback {
    pub fn with_summarizer_chain(mut self, chain: std::sync::Arc<StringSummarizerChain>) -> Self {
        self.summarizer_chain = Some(chain);
        self
    }
}

pub fn default_summarizer_chain() -> StringSummarizerChain {
    StringSummarizerChain::new()
        .register(nix::NixDerivationPreprocessor)
        .register(nix::NixDrvList)
        .register(nix::NixFetchList)
        .register(nix::NixCopyRun)
        .register(nix::NixBuildingRun)
        .register(nix::NixCacheActivity)
        .register(nix::NixImageLayerRun)
        .register(cargo::CargoDownloadRun)
        .register(cargo::CargoCompileRun)
        .register(cargo::CargoCheckRun)
        .register(terraform_install::TerraformInstallRun)
        .register(terraform_state::TerraformRefreshRun)
        .register(terraform_state::TerraformDataSourceRun)
        .register(rsync::RsyncFileList)
        .register(git::GitCloneProgress)
        .register(string_summarizer::BulkElide::default())
}

impl ResultClassifier for GenericFallback {
    fn name(&self) -> &str {
        "default::v1"
    }

    fn matches(&self, _tool_name: &str, _args: &serde_json::Value) -> bool {
        true
    }

    fn classify(&self, ctx: &ClassifierContext<'_>) -> Result<Classification, ClassifierError> {
        let value = canonical_value(ctx.raw);
        let canonical_text = canonical_text_for(&value);
        let chain = self.summarizer_chain.as_deref();
        let summary = match walker::classify_value(&value, self.threshold_bytes, chain) {
            walker::WalkOutcome::Passthrough(v) => ToolResultSummary::Passthrough { value: v },
            walker::WalkOutcome::Elided {
                kept,
                elided_paths,
                total_bytes,
                kept_bytes,
            } => ToolResultSummary::StructuredElision {
                kept,
                elided_paths,
                total_bytes,
                kept_bytes,
            },
        };
        Ok(Classification {
            summary,
            canonical_text,
        })
    }
}

pub fn canonical_text_for(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(map) if map.len() == 1 => match map.values().next() {
            Some(serde_json::Value::String(s)) => s.clone(),
            _ => serde_json::to_string_pretty(value).unwrap_or_default(),
        },
        _ => serde_json::to_string_pretty(value).unwrap_or_default(),
    }
}

fn canonical_value(result: &CallToolResult) -> serde_json::Value {
    if let Some(sc) = result.structured_content.as_ref() {
        return sc.clone();
    }
    serde_json::Value::String(extract_text(result))
}

fn extract_text(result: &CallToolResult) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::Content;

    fn ctx<'a>(tool_name: &'a str, raw: &'a CallToolResult) -> ClassifierContext<'a> {
        static EMPTY_ARGS: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        let args = EMPTY_ARGS.get_or_init(|| serde_json::json!({}));
        ClassifierContext {
            tool_name,
            args,
            raw,
            exit_code: None,
        }
    }

    fn result_with(text: &str) -> CallToolResult {
        CallToolResult::success(vec![Content::text(text.to_string())])
    }

    #[test]
    fn small_output_is_passthrough() {
        let raw = result_with("just a small bit of stdout");
        let registry = ClassifierRegistry::with_default();
        let classification = registry.classify(&ctx("bash", &raw));
        assert!(classification.summary.is_passthrough());
        assert_eq!(classification.canonical_text, "just a small bit of stdout");
    }

    #[test]
    fn large_text_input_emits_structured_elision_with_string_kept() {
        let lines: Vec<String> = (0..1000).map(|i| format!("line-{i:04}")).collect();
        let body = lines.join("\n");
        assert!(body.len() >= DEFAULT_GENERIC_THRESHOLD_BYTES);
        let raw = result_with(&body);
        let registry = ClassifierRegistry::with_default();
        let classification = registry.classify(&ctx("bash", &raw));
        assert_eq!(classification.canonical_text, body);
        match classification.summary {
            ToolResultSummary::StructuredElision {
                kept,
                elided_paths,
                total_bytes,
                kept_bytes,
            } => {
                assert!((kept_bytes as u64) < total_bytes);
                let kept_str = kept.as_str().expect("string-typed kept");
                assert!(kept_str.starts_with("line-0000"));
                assert!(kept_str.ends_with("line-0999"));
                assert_eq!(elided_paths.len(), 1);
                assert_eq!(elided_paths[0].path, "$");
                assert!(matches!(elided_paths[0].kind, ElisionKind::String));
            }
            other => panic!("expected StructuredElision, got {other:?}"),
        }
    }

    #[test]
    fn structured_object_walks_into_oversized_string_field() {
        let huge = "x".repeat(10_000);
        let mut sc = serde_json::Map::new();
        sc.insert("output".into(), serde_json::Value::String(huge.clone()));
        let mut raw = CallToolResult::success(vec![Content::text("ignored")]);
        raw.structured_content = Some(serde_json::Value::Object(sc));

        let registry = ClassifierRegistry::with_default();
        let classification = registry.classify(&ctx("bash", &raw));
        match classification.summary {
            ToolResultSummary::StructuredElision {
                kept, elided_paths, ..
            } => {
                let output = kept
                    .get("output")
                    .and_then(|v| v.as_str())
                    .expect("output key still a JSON string");
                assert!(output.len() < huge.len());
                assert_eq!(elided_paths.len(), 1);
                assert_eq!(elided_paths[0].path, "$.output");
                assert!(matches!(elided_paths[0].kind, ElisionKind::String));
            }
            other => panic!("expected StructuredElision, got {other:?}"),
        }
    }

    #[test]
    fn classifier_failure_falls_through_to_next() {
        struct AlwaysFail;
        impl ResultClassifier for AlwaysFail {
            fn name(&self) -> &str {
                "test::always_fail"
            }
            fn matches(&self, _: &str, _: &serde_json::Value) -> bool {
                true
            }
            fn classify(
                &self,
                _: &ClassifierContext<'_>,
            ) -> Result<Classification, ClassifierError> {
                Err(ClassifierError::Failed {
                    classifier: "test::always_fail",
                    message: "boom".into(),
                })
            }
        }

        let registry = ClassifierRegistry::new()
            .register(AlwaysFail)
            .register(GenericFallback::default());
        let raw = result_with("hi");
        let classification = registry.classify(&ctx("bash", &raw));
        assert!(classification.summary.is_passthrough());
    }

    #[test]
    fn walker_chain_substitutes_in_place_at_string_leaf() {
        let registry = ClassifierRegistry::with_default();
        let mut nix_output = String::from("preparing to build\n");
        nix_output.push_str("building '/nix/store/aaaa-foo.drv'\n");
        for i in 0..20 {
            nix_output.push_str(&format!(
                "copying path '/nix/store/bbb{i:02}-bar' from 'https://cache.nixos.org/'\n"
            ));
        }
        nix_output.push_str(
            "error: builder for '/nix/store/aaaa-foo.drv' failed with exit code 1; last 10 log lines:\n",
        );
        nix_output.push_str("       > some compiler error\n");
        nix_output.push_str("       > another diagnostic line\n");
        let raw = result_with(&nix_output);
        let classification = registry.classify(&ctx("bash", &raw));
        let kept = match classification.summary {
            ToolResultSummary::StructuredElision { kept, .. } => kept,
            ToolResultSummary::Passthrough { value } => value,
            other => panic!("unexpected summary shape: {other:?}"),
        };
        let s = kept.as_str().expect("kept is still Value::String");
        assert!(s.contains("<nix-copy"));
        assert!(s.contains("</nix-copy>"));
        assert!(s.contains("error: builder for"));
        assert!(s.contains("some compiler error"));
    }

    #[test]
    fn threshold_is_configurable() {
        let registry = ClassifierRegistry::new().register(GenericFallback {
            threshold_bytes: 4,
            summarizer_chain: None,
        });
        let body: String = "x".repeat(2000);
        let raw = result_with(&body);
        let classification = registry.classify(&ctx("bash", &raw));
        assert!(matches!(
            classification.summary,
            ToolResultSummary::StructuredElision { .. }
        ));
    }

    #[test]
    fn canonical_text_plain_string_unchanged() {
        let v = serde_json::json!("line1\nline2\nline3");
        assert_eq!(canonical_text_for(&v), "line1\nline2\nline3");
    }

    #[test]
    fn canonical_text_extracts_single_string_field() {
        let v = serde_json::json!({"logs": "first\nsecond\nthird"});
        assert_eq!(canonical_text_for(&v), "first\nsecond\nthird");
    }

    #[test]
    fn canonical_text_pretty_prints_multi_field_object() {
        let v = serde_json::json!({"name": "alice", "age": 30});
        let canonical = canonical_text_for(&v);
        assert!(canonical.contains("\n"));
        assert!(canonical.contains("\"name\""));
        assert!(canonical.contains("\"age\""));
    }

    #[test]
    fn canonical_text_single_field_non_string_falls_back_to_pretty() {
        let v = serde_json::json!({"items": [1, 2, 3]});
        let canonical = canonical_text_for(&v);
        assert!(canonical.contains("\n"));
        assert!(canonical.contains("\"items\""));
    }
}
