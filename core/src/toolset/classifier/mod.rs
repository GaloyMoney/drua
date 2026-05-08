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
mod nix;
mod string_classifier;
mod walker;

pub use concourse::{
    ConcourseBuildLogClassifier, ConcourseBuildLogSummary, ConcourseBuildStatus, NixBuildFailure,
    TimestampedLine,
};
pub use nix::{NixBuildSummary, NixDerivationFailure, NixStringClassifier};
pub use string_classifier::{StringClassifier, StringClassifierChain};

/// Default byte threshold for [`GenericFallback`]. Below → `Passthrough`;
/// at-or-above → the JSON-aware walker emits `StructuredElision`. 4 KB is
/// roughly aligned with the brief's `019e019e` cold-shell pathology;
/// tunable per-deployment by constructing `GenericFallback` with a
/// custom `threshold_bytes`.
pub const DEFAULT_GENERIC_THRESHOLD_BYTES: usize = 4096;

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
    /// Renamed from `Concourse` because Concourse is the upstream
    /// service, not a content shape — future Concourse-shaped outputs
    /// (resource versions, pipeline config) would each get their own
    /// kind.
    ConcourseLogs(ConcourseBuildLogSummary),
}

impl ToolResultSummary {
    /// `kind` discriminator used by the dispatcher to decide whether to
    /// persist + envelope. **Must match the serde tag exactly** —
    /// the `classifier` DB column stores `kind()` while the
    /// `summary` JSONB column stores the serde-tagged form;
    /// disagreement makes those two columns disagree about what
    /// kind of summary the row actually carries (cursor review
    /// #3207287028). Variants follow `rename_all = "snake_case"`
    /// so e.g. `ConcourseLogs` serialises as `"concourse_logs"`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Passthrough { .. } => "passthrough",
            Self::StructuredElision { .. } => "structured_elision",
            Self::ConcourseLogs(_) => "concourse_logs",
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
    /// Re-enter the registry to classify a sub-region of `raw`. Parent
    /// classifiers (e.g. `ConcourseLogsClassifier` extracting a failed
    /// derivation's log_tail) call this on the substring; matching
    /// inner classifiers fire via `matches_content` and produce a
    /// nested `ToolResultSummary` for the parent to embed. `None` when
    /// no inner classifier matched (the caller stores raw text). The
    /// region recursion is bounded — each region is strictly smaller
    /// than its parent's input.
    pub classify_region: &'a dyn Fn(&str) -> Option<ToolResultSummary>,
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
    /// Identity-based fast-path match. Cheap; runs first.
    fn matches(&self, tool_name: &str, args: &serde_json::Value) -> bool;
    /// Content-sniff fallback. Runs only when no classifier matched
    /// by identity, and when the parent is a sub-region recursion
    /// (where there is no meaningful tool_name). Default: never.
    /// Override to peek at the raw bytes via `ctx.raw` (e.g. regex
    /// over `extract_text(ctx.raw)`).
    fn matches_content(&self, _ctx: &ClassifierContext<'_>) -> bool {
        false
    }
    /// `true` for the registry's last-resort classifier. The dispatcher
    /// skips catch-all classifiers in the identity and content-sniff
    /// passes and runs them only after every more-specific classifier
    /// has declined. Without this, a `matches`-returns-`true` catch-all
    /// would win the identity pass and short-circuit the content-sniff
    /// pass for every input.
    fn is_catch_all(&self) -> bool {
        false
    }
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
    /// Identity-matchers (concourse) run first; content-sniff
    /// classifiers (Nix) run between identity and `GenericFallback`.
    /// `GenericFallback` carries a `StringClassifierChain` (NixString,
    /// future Cargo / Nextest / cpp-compile) consulted at every string
    /// leaf during walk.
    pub fn with_default() -> Self {
        let string_chain = std::sync::Arc::new(
            StringClassifierChain::new().register(NixStringClassifier),
        );
        Self::new()
            .register(ConcourseBuildLogClassifier)
            .register(GenericFallback::default().with_string_classifiers(string_chain))
    }

    pub fn register(mut self, classifier: impl ResultClassifier) -> Self {
        self.classifiers.push(Box::new(classifier));
        self
    }

    pub fn classify(&self, ctx: &ClassifierContext<'_>) -> Classification {
        // First pass — identity match (cheap, declarative). Catch-all
        // classifiers are skipped here: their always-true `matches`
        // would short-circuit the content-sniff pass.
        for classifier in &self.classifiers {
            if classifier.is_catch_all() {
                continue;
            }
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
        // Second pass — content-sniff. Cargo / Nix / Nextest
        // classifiers use this path to fire on bash-disguised content
        // and on sub-region recursion. Catch-alls also skipped here.
        for classifier in &self.classifiers {
            if classifier.is_catch_all() {
                continue;
            }
            if !classifier.matches_content(ctx) {
                continue;
            }
            match classifier.classify(ctx) {
                Ok(c) => return c,
                Err(e) => {
                    tracing::warn!(
                        classifier = classifier.name(),
                        error = %e,
                        "content-sniff classifier failed; trying next",
                    );
                }
            }
        }
        // Third pass — catch-all (GenericFallback). Last resort.
        for classifier in &self.classifiers {
            if !classifier.is_catch_all() {
                continue;
            }
            if let Ok(c) = classifier.classify(ctx) {
                return c;
            }
        }
        // Empty registry safety net.
        let canonical_text = extract_text(ctx.raw);
        Classification {
            summary: ToolResultSummary::Passthrough {
                value: serde_json::Value::String(canonical_text.clone()),
            },
            canonical_text,
        }
    }

    /// Classify a substring of a parent's `raw_text` as a candidate
    /// embedded summary. Used by parent classifiers (Concourse →
    /// failed derivation log tail → optional Cargo/Nix/etc. inner
    /// summary). Only the content-sniff pass runs — identity-based
    /// match is meaningless for an anonymous text region. Returns
    /// `None` when no classifier matched (the parent stores the
    /// region as raw text instead).
    pub fn classify_region(&self, region: &str) -> Option<ToolResultSummary> {
        // Synthesize a minimal CallToolResult so existing classifiers
        // can read the region via their normal `extract_text(ctx.raw)`
        // helper. structured_content is left None, mimicking a plain-
        // text tool's call result.
        use rmcp::model::Content;
        let synthetic = CallToolResult::success(vec![Content::text(region.to_string())]);
        let no_args = serde_json::Value::Object(serde_json::Map::new());
        let nop_recurse: &dyn Fn(&str) -> Option<ToolResultSummary> = &|_| None;
        let region_ctx = ClassifierContext {
            tool_name: "",
            args: &no_args,
            raw: &synthetic,
            exit_code: None,
            classify_region: nop_recurse,
        };
        for classifier in &self.classifiers {
            // Catch-alls would always claim regions and defeat the
            // `Option<...>` "no inner classifier matched" semantics.
            if classifier.is_catch_all() {
                continue;
            }
            if !classifier.matches_content(&region_ctx) {
                continue;
            }
            if let Ok(c) = classifier.classify(&region_ctx) {
                return Some(c.summary);
            }
        }
        None
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
    /// Consulted at every string leaf in the walker. First match
    /// substitutes the string with a typed sentinel (`{"_typed": ...,
    /// "summary": ...}`); no match falls through to the existing
    /// byte-elision behaviour. `None` keeps the legacy behaviour
    /// (no typed strings).
    pub string_classifiers: Option<std::sync::Arc<StringClassifierChain>>,
}

impl Default for GenericFallback {
    fn default() -> Self {
        Self {
            threshold_bytes: DEFAULT_GENERIC_THRESHOLD_BYTES,
            string_classifiers: None,
        }
    }
}

impl GenericFallback {
    pub fn with_string_classifiers(mut self, chain: std::sync::Arc<StringClassifierChain>) -> Self {
        self.string_classifiers = Some(chain);
        self
    }
}

impl ResultClassifier for GenericFallback {
    fn name(&self) -> &str {
        "default::v1"
    }

    fn matches(&self, _tool_name: &str, _args: &serde_json::Value) -> bool {
        true
    }

    fn is_catch_all(&self) -> bool {
        true
    }

    fn classify(&self, ctx: &ClassifierContext<'_>) -> Result<Classification, ClassifierError> {
        let value = canonical_value(ctx.raw);
        // canonical_text is the bytes `tool_output_fetch` will operate
        // on. Goal: a multi-line representation where line-oriented
        // grep is meaningful. Three shapes:
        //
        // - `Value::String(s)` — plain-text tools (bash, k8s logs).
        //   Use the string verbatim; agents grep over real lines.
        // - Single-string-field object like `{"logs": "<multi-line>"}`
        //   (concourse, k8s pod logs, github raw fetch, bash via
        //   tunnel). Compact JSON would collapse the embedded `\n`s
        //   into the literal two-character sequence and grep would
        //   see the whole document as one line. Extract the inner
        //   string so each newline is a real line break.
        // - Anything else — pretty-printed JSON. Multi-line, grep-able
        //   on keys + values; less optimal than a typed classifier
        //   but useful as a fallback.
        let canonical_text = canonical_text_for(&value);
        let chain = self.string_classifiers.as_deref();
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

/// Render the canonical text representation of a JSON value for
/// downstream grep-mode fetches. See `GenericFallback::classify`
/// for the rationale on each branch.
pub(super) fn canonical_text_for(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(map) if map.len() == 1 => match map.values().next() {
            Some(serde_json::Value::String(s)) => s.clone(),
            _ => serde_json::to_string_pretty(value).unwrap_or_default(),
        },
        _ => serde_json::to_string_pretty(value).unwrap_or_default(),
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
        // No-op region recursion — most tests don't care; the few that
        // exercise nesting build their own context.
        static NO_RECURSE: fn(&str) -> Option<ToolResultSummary> = |_| None;
        ClassifierContext {
            tool_name,
            args,
            raw,
            exit_code: None,
            classify_region: &NO_RECURSE,
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
    fn walker_chain_embeds_typed_sentinel_at_string_leaf() {
        // The new "walker is the spine, string classifiers fire at
        // leaves" path: a bash result whose stdout looks like nix
        // output should land in `StructuredElision { kept }` where
        // `kept` is a typed sentinel (`{"_typed": "nix_build", ...}`)
        // instead of a byte-elided string. No identity match for
        // `bash`; the chain is the only signal.
        let registry = ClassifierRegistry::with_default();
        let nix_output = "\
preparing to build\n\
building '/nix/store/aaaa-foo.drv'\n\
copying path '/nix/store/bbbb-bar' from 'https://cache.nixos.org/'\n\
copying path '/nix/store/cccc-baz' from 'https://cache.nixos.org/'\n\
error: builder for '/nix/store/aaaa-foo.drv' failed with exit code 1; last 10 log lines:\n\
       > some compiler error\n\
       > another diagnostic line\n";
        let raw = result_with(nix_output);
        let classification = registry.classify(&ctx("bash", &raw));
        let kept = match classification.summary {
            ToolResultSummary::StructuredElision { kept, .. } => kept,
            other => panic!(
                "expected StructuredElision with typed-sentinel kept, got {other:?}"
            ),
        };
        // The walker chain should have substituted the root string
        // with a Nix-typed sentinel.
        let typed = kept
            .get("_typed")
            .and_then(|v| v.as_str())
            .expect("kept should be a typed sentinel");
        assert_eq!(typed, "nix_build");
        let summary = kept
            .get("summary")
            .expect("typed sentinel carries the summary inline");
        assert_eq!(
            summary
                .get("derivations_attempted")
                .and_then(|v| v.as_u64()),
            Some(1)
        );
        assert_eq!(
            summary
                .get("cache_paths_copied")
                .and_then(|v| v.as_u64()),
            Some(2)
        );
    }

    #[test]
    fn threshold_is_configurable() {
        // A custom 4-byte threshold makes any non-trivial input trip
        // elision; the walker emits StructuredElision when the post-
        // walk form is smaller than the input.
        let registry = ClassifierRegistry::new().register(GenericFallback {
            threshold_bytes: 4,
            string_classifiers: None,
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
        // {"logs": "<multi-line>"} is the universal shape for log-emitting
        // structured tools (concourse, k8s pod logs, github raw, bash via
        // tunnel). Without this carve-out, grep against the persisted
        // raw_text sees one line — the JSON envelope.
        let v = serde_json::json!({"logs": "first\nsecond\nthird"});
        assert_eq!(canonical_text_for(&v), "first\nsecond\nthird");
    }

    #[test]
    fn canonical_text_pretty_prints_multi_field_object() {
        let v = serde_json::json!({"name": "alice", "age": 30});
        let canonical = canonical_text_for(&v);
        // Pretty-print produces line-per-field — grep can match keys
        // and values independently.
        assert!(canonical.contains("\n"), "pretty-printed: {canonical}");
        assert!(canonical.contains("\"name\""), "{canonical}");
        assert!(canonical.contains("\"age\""), "{canonical}");
    }

    #[test]
    fn canonical_text_single_field_non_string_falls_back_to_pretty() {
        let v = serde_json::json!({"items": [1, 2, 3]});
        let canonical = canonical_text_for(&v);
        // Single-field heuristic is restricted to string values; a
        // single-field object whose value is e.g. an array should
        // pretty-print so the array's elements are line-grep-able.
        assert!(canonical.contains("\n"), "{canonical}");
        assert!(canonical.contains("\"items\""), "{canonical}");
    }
}
