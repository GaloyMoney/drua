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

pub use concourse::{
    ConcourseBuildLogClassifier, ConcourseBuildLogSummary, ConcourseBuildStatus, NixBuildFailure,
    TimestampedLine,
};

/// Default byte threshold for [`GenericFallback`]. Below → `Passthrough`;
/// at-or-above → `Generic { head, tail, … }`. 4 KB is roughly aligned with
/// the brief's `019e019e` cold-shell pathology; tunable per-deployment by
/// constructing `GenericFallback` with a custom `threshold_bytes`.
pub const DEFAULT_GENERIC_THRESHOLD_BYTES: usize = 4096;

/// Lines kept from the head and tail when generic-eliding. Picked to mirror
/// the Cline / OpenHands shipping behaviour (preserve both ends; drop
/// middle).
const GENERIC_HEAD_LINES: usize = 50;
const GENERIC_TAIL_LINES: usize = 150;

/// What the classifier produces. The dispatcher branches on the variant.
///
/// New variants land here as classifiers come online (`CargoBuild`,
/// `CargoTest`, `NixBuild`, `Concourse`, `Diff`, …). The discriminator is
/// stable enough to persist into `tool_invocations.summary` JSONB without
/// migration churn.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolResultSummary {
    /// Below the threshold and no typed classifier opted in — the agent
    /// already received the full output, no envelope is added.
    Passthrough { text: String },

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
            Self::Concourse(_) => "concourse_build_log",
        }
    }

    /// Fast-path test — true means the dispatcher emits the raw result
    /// unchanged.
    pub fn is_passthrough(&self) -> bool {
        matches!(self, Self::Passthrough { .. })
    }
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

pub trait ResultClassifier: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn matches(&self, tool_name: &str, args: &serde_json::Value) -> bool;
    fn classify(&self, ctx: &ClassifierContext<'_>) -> Result<ToolResultSummary, ClassifierError>;
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

    pub fn classify(&self, ctx: &ClassifierContext<'_>) -> ToolResultSummary {
        for classifier in &self.classifiers {
            if !classifier.matches(ctx.tool_name, ctx.args) {
                continue;
            }
            match classifier.classify(ctx) {
                Ok(summary) => return summary,
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
        ToolResultSummary::Passthrough {
            text: extract_text(ctx.raw),
        }
    }
}

impl Default for ClassifierRegistry {
    fn default() -> Self {
        Self::with_default()
    }
}

/// Last-resort classifier. Always matches; threshold-gates between
/// `Passthrough` (fast path) and `Generic` (head + tail, middle elided).
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
        "generic::v1"
    }

    fn matches(&self, _tool_name: &str, _args: &serde_json::Value) -> bool {
        true
    }

    fn classify(&self, ctx: &ClassifierContext<'_>) -> Result<ToolResultSummary, ClassifierError> {
        let raw = extract_text(ctx.raw);
        if raw.len() < self.threshold_bytes {
            return Ok(ToolResultSummary::Passthrough { text: raw });
        }

        let lines: Vec<&str> = raw.lines().collect();
        let head = lines
            .iter()
            .take(GENERIC_HEAD_LINES)
            .copied()
            .collect::<Vec<_>>()
            .join("\n");
        let tail_start = lines.len().saturating_sub(GENERIC_TAIL_LINES);
        let tail = lines[tail_start..].join("\n");
        let kept_bytes = (head.len() + tail.len()) as u32;
        Ok(ToolResultSummary::Generic {
            head,
            tail,
            total_bytes: raw.len() as u64,
            kept_bytes,
            classifier_hint: None,
        })
    }
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
        let summary = registry.classify(&ctx("bash", &raw));
        assert!(summary.is_passthrough(), "got: {:?}", summary);
    }

    #[test]
    fn large_output_falls_to_generic_with_head_and_tail() {
        let lines: Vec<String> = (0..500).map(|i| format!("line-{i:04}")).collect();
        let body = lines.join("\n");
        assert!(
            body.len() >= DEFAULT_GENERIC_THRESHOLD_BYTES,
            "test body should exceed default threshold"
        );
        let raw = result_with(&body);
        let registry = ClassifierRegistry::with_default();
        let summary = registry.classify(&ctx("bash", &raw));
        match summary {
            ToolResultSummary::Generic {
                head,
                tail,
                total_bytes,
                kept_bytes,
                ..
            } => {
                assert_eq!(total_bytes, body.len() as u64);
                assert!(kept_bytes > 0);
                assert!(head.starts_with("line-0000"));
                assert!(tail.ends_with("line-0499"));
                assert_eq!(head.lines().count(), GENERIC_HEAD_LINES);
                assert_eq!(tail.lines().count(), GENERIC_TAIL_LINES);
            }
            other => panic!("expected Generic, got {other:?}"),
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
            ) -> Result<ToolResultSummary, ClassifierError> {
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
        let summary = registry.classify(&ctx("bash", &raw));
        assert!(summary.is_passthrough());
    }

    #[test]
    fn threshold_is_configurable() {
        let registry = ClassifierRegistry::new().register(GenericFallback { threshold_bytes: 4 });
        let raw = result_with("ten chars\n");
        let summary = registry.classify(&ctx("bash", &raw));
        assert!(matches!(summary, ToolResultSummary::Generic { .. }));
    }
}
