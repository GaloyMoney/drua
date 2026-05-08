//! `ResultClassifier` plus the universal `GenericFallback` and
//! per-tool classifiers (currently: concourse build logs).
//! Real per-tool classifiers for the rest of the catalog (cargo, nextest,
//! nix, kubernetes, …) land in follow-up PRs.
//!
//! The dispatcher branches on the variant emitted by `classify`:
//! `Passthrough` keeps the fast path (no envelope, no persistence); every
//! other variant triggers the universal envelope. The threshold gating
//! lives inside `GenericFallback`, not as a separate dispatcher gate, so
//! tool-specific classifiers can opt to wrap unconditionally — a typed
//! shape is the value, even at small sizes.

use rmcp::model::CallToolResult;
use serde::{Deserialize, Serialize};

mod concourse;
mod walker;

pub use concourse::{
    ConcourseBuildLogClassifier, ConcourseBuildLogSummary, ConcourseBuildStatus, NixBuildFailure,
    TimestampedLine,
};

/// Default byte threshold for [`GenericFallback`]. Below → `Passthrough`;
/// at-or-above → `Generic { head, tail, … }`. 4 KB is roughly aligned with
/// the brief's `019e019e` cold-shell pathology; tunable per-deployment by
/// constructing `GenericFallback` with a custom `threshold_bytes`.
pub const DEFAULT_GENERIC_THRESHOLD_BYTES: usize = 4096;

// Pre-walker line-budget constants are gone — the walker uses
// char-budget string elision instead, which doesn't suffer the
// head/tail overlap class of bug. See `walker::STRING_ELIDE_*` for
// the new caps.

/// What the classifier produces. The dispatcher branches on the variant.
///
/// New variants land here as classifiers come online (`CargoBuild`,
/// `CargoTest`, `NixBuild`, `Concourse`, `Diff`, …). The discriminator is
/// stable enough to persist into `tool_invocations.summary` JSONB without
/// migration churn.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolResultSummary {
    /// Below the threshold and no typed classifier opted in — the
    /// agent already received the full output, no envelope is added.
    /// Carries `Value` rather than `String` so structured tool
    /// results (compose, github_get_pr, honeycomb queries) keep
    /// their programmatic shape end-to-end. Plain-text tools land
    /// here as `Value::String(text)`; the old `text` field is
    /// recoverable with `value.as_str()`.
    Passthrough { value: serde_json::Value },

    /// Generic head/tail/middle-elision over the threshold. Lossy in the
    /// middle by design; the dropped detail is recoverable via the
    /// persisted `tool_invocations` row + `tool_output_fetch`.
    Generic {
        head: String,
        tail: String,
        total_bytes: u64,
        kept_bytes: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        classifier_hint: Option<String>,
    },

    /// JSON-aware elision. The walker preserves shape — strings get
    /// byte-elided in place (type stays `String`), arrays/objects too
    /// big to walk into get replaced with typed sentinels in `kept`.
    /// Each elided branch is also enumerated in `elided_paths` so the
    /// agent can reason about what was dropped without inspecting the
    /// kept structure.
    StructuredElision {
        /// Partial JSON value — always parseable. Branches that didn't
        /// fit the budget appear as `{"_elided": true, "kind": "...",
        /// "bytes": ..., ...}` sentinels (see `kept` walker rules).
        kept: serde_json::Value,
        elided_paths: Vec<ElidedPath>,
        total_bytes: u64,
        kept_bytes: u32,
    },

    /// Typed summary of a Concourse build's text log. Collapses ~100 KB
    /// of timestamped progress (nix substituter chatter, derivation
    /// checks, cache pruning) into a structured shape that preserves
    /// every signal-bearing line — warnings, build failures, the full
    /// nix log tail of any failed derivation, and the closing N lines.
    Concourse(ConcourseBuildLogSummary),
}

impl ToolResultSummary {
    /// `kind` discriminator used by the dispatcher to decide whether to
    /// persist + envelope. Mirrors the serde tag.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Passthrough { .. } => "passthrough",
            Self::Generic { .. } => "generic",
            Self::StructuredElision { .. } => "structured_elision",
            Self::Concourse(_) => "concourse_build_log",
        }
    }

    /// Fast-path test — true means the dispatcher emits the raw result
    /// unchanged.
    pub fn is_passthrough(&self) -> bool {
        matches!(self, Self::Passthrough { .. })
    }
}

/// One branch of a `StructuredElision`'s `kept` value that the walker
/// replaced with a sentinel. Recorded so the agent can scan
/// `elided_paths` to discover what was dropped without recursing
/// through `kept` to find sentinels.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElidedPath {
    /// JSON-pointer-style path: `$.steps[12].log`,
    /// `$.builds`, `$.["weird key"]`. Roots at `$`.
    pub path: String,
    pub kind: ElisionKind,
    pub bytes: u64,
    /// For arrays — element count of the original branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<usize>,
    /// First ~200 chars of a string sentinel, head sample of an
    /// array sentinel, etc. — small enough to inline without
    /// blowing the budget. `None` when the walker had no preview
    /// budget left.
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

/// What a classifier returns. Bundles the typed summary with the bytes
/// the summary's offsets refer to so the dispatcher persists exactly
/// what the classifier saw — fixes drift between classifier-side text
/// extraction (e.g. Concourse reading `structured_content.logs`) and
/// dispatcher-side persistence (which used to re-extract from
/// `content[].text`). After this trait shape, `tool_output_fetch` is
/// guaranteed to return bytes whose offsets match the summary's
/// `head`/`tail` slicing.
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

/// First-match dispatch. Order matters — most-specific classifiers go first;
/// `GenericFallback` is registered last so it always catches anything earlier
/// classifiers declined.
pub struct ClassifierRegistry {
    classifiers: Vec<Box<dyn ResultClassifier>>,
}

impl ClassifierRegistry {
    pub fn new() -> Self {
        Self {
            classifiers: Vec::new(),
        }
    }

    /// Pre-built registry with every shipping classifier registered in
    /// most-specific → fallback order. New classifiers slot in here.
    pub fn with_default() -> Self {
        Self::new()
            .register(ConcourseBuildLogClassifier)
            .register(GenericFallback::default())
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
        // Unreachable when GenericFallback is registered, but produce a
        // safe default if someone hands us an empty registry.
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

/// Last-resort classifier. Always matches; runs the JSON-aware
/// walker over a `Value` reconstructed from the call result.
///
/// The walker handles both shapes uniformly:
/// - Plain-text tools (bash, k8s logs) arrive with no
///   `structured_content`; the helper wraps `content[].text` as
///   `Value::String(text)`. The walker's string branch byte-elides
///   in place — the kept value is still a `String`, agents reading
///   `summary.value.as_str()` keep working.
/// - Structured tools (compose, github_get_pr) carry
///   `structured_content` directly. The walker descends, byte-eliding
///   leaf strings and sentinel-replacing oversize collections only
///   after recursion has had a chance to shrink them.
///
/// `output_schema` is irrelevant at runtime — declared shape is for
/// registration-time contracts (workflow `ToolStep` validator).
pub struct GenericFallback {
    pub threshold_bytes: usize,
}

impl Default for GenericFallback {
    fn default() -> Self {
        Self {
            threshold_bytes: DEFAULT_GENERIC_THRESHOLD_BYTES,
        }
    }
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
        // canonical_text is the bytes `tool_output_fetch` will operate
        // on. For Value::String we persist the inner string content
        // (no JSON quotes) so bash-style fetches grep over the
        // original bytes, not their stringified form. For everything
        // else we serialize compactly.
        let canonical_text = match &value {
            serde_json::Value::String(s) => s.clone(),
            other => serde_json::to_string(other).unwrap_or_default(),
        };
        let summary = match walker::classify_value(&value, self.threshold_bytes) {
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

/// Build a `Value` from the call result. `structured_content` wins
/// when set; otherwise the joined content text is wrapped as
/// `Value::String` so the walker has a single uniform input shape.
fn canonical_value(result: &CallToolResult) -> serde_json::Value {
    if let Some(sc) = result.structured_content.as_ref() {
        return sc.clone();
    }
    serde_json::Value::String(extract_text(result))
}

/// Extract concatenated text content from a `CallToolResult`. Mirrors the
/// helper in `filter.rs` — duplicated here to avoid exporting it crate-wide.
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
        // Static reference required by ClassifierContext lifetime; for
        // tests the args value is just an empty object.
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
        assert!(
            classification.summary.is_passthrough(),
            "got: {:?}",
            classification.summary
        );
        assert_eq!(classification.canonical_text, "just a small bit of stdout");
    }

    #[test]
    fn large_text_input_emits_structured_elision_with_string_kept() {
        // Plain-text tools (no `structured_content`) arrive as
        // `Value::String(text)` after `canonical_value`. The walker
        // keeps the type as `String` — head + ellipsis + tail in
        // place — so agents reading `summary.kept.as_str()` keep
        // working.
        let lines: Vec<String> = (0..500).map(|i| format!("line-{i:04}")).collect();
        let body = lines.join("\n");
        assert!(
            body.len() >= DEFAULT_GENERIC_THRESHOLD_BYTES,
            "test body should exceed default threshold"
        );
        let raw = result_with(&body);
        let registry = ClassifierRegistry::with_default();
        let classification = registry.classify(&ctx("bash", &raw));
        // canonical_text is the bytes the summary's offsets refer to —
        // exactly what the dispatcher will persist.
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
                assert!(kept_str.ends_with("line-0499"));
                assert_eq!(elided_paths.len(), 1);
                assert_eq!(elided_paths[0].path, "$");
                assert!(matches!(elided_paths[0].kind, ElisionKind::String));
            }
            other => panic!("expected StructuredElision, got {other:?}"),
        }
    }

    #[test]
    fn structured_object_walks_into_oversized_string_field() {
        // Bash-shape: `{"output": "<huge text>"}`. The walker keeps
        // the wrapper object verbatim; the inner string is byte-elided
        // in place so `kept.output` is still a JSON string.
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
                assert!(output.len() < huge.len(), "output should be byte-elided");
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
    fn threshold_is_configurable() {
        // A custom 4-byte threshold makes any non-trivial input trip
        // elision; the walker emits StructuredElision when the post-
        // walk form is smaller than the input.
        let registry = ClassifierRegistry::new().register(GenericFallback { threshold_bytes: 4 });
        let body: String = "x".repeat(2000);
        let raw = result_with(&body);
        let classification = registry.classify(&ctx("bash", &raw));
        assert!(matches!(
            classification.summary,
            ToolResultSummary::StructuredElision { .. }
        ));
    }
}
