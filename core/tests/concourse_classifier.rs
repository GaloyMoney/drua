//! Drives the [`ConcourseBuildLogClassifier`] against captured fixtures from
//! ci.galoy.io's `galoy-agents-bin` pipeline (`check-code` job, builds #610
//! and #609). The fixtures are committed under
//! `core/tests/fixtures/concourse/` so the test runs offline and stays
//! reproducible across PG resets.
//!
//! Build #610 succeeded (status check); the only signal-bearing lines are
//! a handful of `warning:` rows and the closing tail. Build #609 failed
//! during `cargo clippy` inside `nix flake check`, with the rust diagnostic
//! captured in the indented `> ...` log tail.
//!
//! Running this test demonstrates the full pipeline end-to-end:
//! ~108 KB of raw concourse text in → ~2 KB of typed summary out, with
//! every actionable line preserved.

use rmcp::model::{CallToolResult, Content};

use std::sync::Arc;

use drua_core::toolset::{
    ClassifierContext, ClassifierRegistry, ConcourseBuildLogClassifier, ConcourseBuildStatus,
    NixStringClassifier, ResultClassifier, StringClassifierChain, ToolResultSummary,
};

const BUILD_610_SUCCEEDED: &str = include_str!("fixtures/concourse/build-610-succeeded.log");
const BUILD_609_FAILED: &str = include_str!("fixtures/concourse/build-609-failed.log");

fn classify(raw: &str) -> ToolResultSummary {
    let mut result = CallToolResult::success(vec![Content::text(raw.to_string())]);
    result.structured_content = Some(serde_json::json!({"logs": raw}));
    let args = serde_json::json!({"build_id": 7001970u64});
    let ctx = ClassifierContext {
        tool_name: "concourse_get_build_logs",
        args: &args,
        raw: &result,
        exit_code: None,
    };
    // Wire the StringClassifierChain so `inner` gets populated by
    // NixStringClassifier — same shape `with_default()` produces.
    let chain = Arc::new(StringClassifierChain::new().register(NixStringClassifier));
    ConcourseBuildLogClassifier::new(chain)
        .classify(&ctx)
        .expect("classifier never errors on valid input")
        .summary
}

/// Helper: extract the typed nix_build summary from a Concourse
/// summary's `inner` field (set by the walker chain). Panics if the
/// inner isn't a `nix_build` typed sentinel.
fn nix_inner(s: &drua_core::toolset::ConcourseBuildLogSummary) -> &serde_json::Value {
    let inner = &s.inner;
    let kind = inner
        .get("_typed")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("inner should be a typed sentinel, got {inner:#}"));
    assert_eq!(kind, "nix_build", "expected nix_build inner; got {inner:#}");
    inner
        .get("summary")
        .unwrap_or_else(|| panic!("typed sentinel must carry summary; got {inner:#}"))
}

#[test]
fn build_610_succeeded_extracts_meta_and_delegates_inner_to_walker() {
    let raw_size = BUILD_610_SUCCEEDED.len();
    assert!(
        raw_size > 100_000,
        "fixture should be large enough to demonstrate compression (got {raw_size} bytes)"
    );

    let summary = match classify(BUILD_610_SUCCEEDED) {
        ToolResultSummary::ConcourseLogs(s) => s,
        other => panic!("expected ConcourseLogs summary, got {other:?}"),
    };

    // Concourse-level signals: succeeded build → no failure pattern,
    // no stray errors, status NoFailureDetected.
    assert_eq!(summary.status, ConcourseBuildStatus::NoFailureDetected);
    assert!(
        summary.errors.is_empty(),
        "succeeded build should have no stray error: lines"
    );
    // Five distinct `warning:` lines in the fixture; preserved verbatim
    // at the concourse layer (warnings can come from any tool, but
    // they're useful as a top-level signal).
    assert_eq!(summary.warnings.len(), 5);
    assert!(summary
        .warnings
        .iter()
        .any(|w| w.message.contains("apps.x86_64-linux.bats")));
    assert!(!summary.task_phases.is_empty());
    assert!(summary
        .final_lines
        .iter()
        .any(|l| l.contains("done saving")));

    // Inner: walker chain matched NixStringClassifier on the log
    // content. Derivation counts and cache copies live there now.
    let nix = nix_inner(&summary);
    let cache_copies = nix
        .get("cache_paths_copied")
        .and_then(|v| v.as_u64())
        .expect("cache_paths_copied present");
    assert!(
        cache_copies >= 700,
        "expected ≥700 cache copies in 610 fixture; got {cache_copies}"
    );
    let derivs = nix
        .get("derivations_attempted")
        .and_then(|v| v.as_u64())
        .expect("derivations_attempted present");
    assert!(
        derivs >= 1,
        "expected ≥1 derivation attempted; got {derivs}"
    );

    eprintln!(
        "build #610: {} bytes raw → concourse meta + nix inner ({} warnings, \
         {} cache copies, {} derivations)",
        summary.total_bytes,
        summary.warnings.len(),
        cache_copies,
        derivs,
    );
}

#[test]
fn build_609_failed_inner_carries_clippy_rust_diagnostic() {
    let summary = match classify(BUILD_609_FAILED) {
        ToolResultSummary::ConcourseLogs(s) => s,
        other => panic!("expected ConcourseLogs summary, got {other:?}"),
    };

    // Concourse layer: status escalated to Failed because at least one
    // `error: builder for ...failed` line was seen (or stray errors).
    assert_eq!(summary.status, ConcourseBuildStatus::Failed);

    // Inner (walker chain → NixStringClassifier): derivation failure
    // detail lives here.
    let nix = nix_inner(&summary);
    let failures = nix
        .get("failures")
        .and_then(|v| v.as_array())
        .expect("nix inner should expose failures");
    assert_eq!(
        failures.len(),
        1,
        "exactly one failed derivation in fixture"
    );

    let f = &failures[0];
    let drv = f
        .get("drv_path")
        .and_then(|v| v.as_str())
        .expect("drv_path present");
    assert!(
        drv.contains("drua-clippy-0.1.0.drv"),
        "expected clippy drv path, got {drv:?}"
    );
    let log_tail = f
        .get("log_tail")
        .and_then(|v| v.as_array())
        .expect("log_tail present");
    let concat = log_tail
        .iter()
        .filter_map(|l| l.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        concat.contains("error[E0063]: missing fields"),
        "rust diagnostic must survive into the inner nix summary; got: {concat:?}"
    );
    assert!(concat.contains("McpUpstreamConfig"));
    assert!(concat.contains("auth_mode"));
    assert!(concat.contains("internal_only"));

    eprintln!(
        "build #609: {} bytes raw → concourse meta + nix inner with {} failures",
        summary.total_bytes,
        failures.len(),
    );
}

#[test]
fn registry_routes_concourse_to_typed_classifier() {
    // Sanity that `with_default()` registers concourse before the
    // GenericFallback so classification doesn't accidentally fall through
    // to head/tail elision and lose the typed shape.
    let registry = ClassifierRegistry::with_default();
    let mut result = CallToolResult::success(vec![Content::text(BUILD_610_SUCCEEDED.to_string())]);
    result.structured_content = Some(serde_json::json!({"logs": BUILD_610_SUCCEEDED}));

    let args = serde_json::json!({"build_id": 7001970u64});
    let ctx = ClassifierContext {
        tool_name: "concourse_get_build_logs",
        args: &args,
        raw: &result,
        exit_code: None,
    };
    let classification = registry.classify(&ctx);
    assert_eq!(classification.summary.kind(), "concourse_logs");
}

/// End-to-end: a `bash`-shape result whose stdout looks like a nix
/// build lands in `StructuredElision { kept: <typed sentinel> }`
/// where the sentinel is the `NixStringClassifier`'s output. No
/// identity match for `bash` — the walker descends to the root
/// string, the chain matches via content sniff, and the typed shape
/// is embedded inline.
#[test]
fn nix_string_classifier_fires_on_content_sniff_top_level() {
    let nix_output = "\
preparing to build /nix/store/aaaa-foo.drv
building '/nix/store/aaaa-foo.drv'
copying path '/nix/store/bbbb-bar' from 'https://cache.nixos.org/'
copying path '/nix/store/cccc-baz' from 'https://cache.nixos.org/'
error: builder for '/nix/store/aaaa-foo.drv' failed with exit code 1; last 10 log lines:
       > some compiler error
       > another diagnostic line
";

    let registry = ClassifierRegistry::with_default();
    let result = CallToolResult::success(vec![Content::text(nix_output.to_string())]);
    let args = serde_json::json!({"command": "nix build .#foo"});
    let ctx = ClassifierContext {
        tool_name: "bash",
        args: &args,
        raw: &result,
        exit_code: None,
    };

    let classification = registry.classify(&ctx);
    let kept = match classification.summary {
        ToolResultSummary::StructuredElision { kept, .. } => kept,
        other => panic!("expected StructuredElision with typed sentinel, got {other:?}"),
    };
    assert_eq!(
        kept.get("_typed").and_then(|v| v.as_str()),
        Some("nix_build"),
        "kept should be a NixStringClassifier sentinel"
    );
    let summary = kept.get("summary").expect("typed sentinel carries summary");
    assert_eq!(
        summary
            .get("derivations_attempted")
            .and_then(|v| v.as_u64()),
        Some(1)
    );
    assert_eq!(
        summary.get("cache_paths_copied").and_then(|v| v.as_u64()),
        Some(2)
    );
    assert_eq!(
        summary
            .get("failures")
            .and_then(|v| v.as_array())
            .map(|a| a.len()),
        Some(1)
    );
}

/// Confirms unrelated bash output (no nix shape) doesn't accidentally
/// trip the chain — `ls`-style output passes through.
#[test]
fn unrelated_bash_output_passes_through() {
    let registry = ClassifierRegistry::with_default();
    let result = CallToolResult::success(vec![Content::text("file1.txt\nfile2.txt\nfile3.txt")]);
    let args = serde_json::json!({"command": "ls"});
    let ctx = ClassifierContext {
        tool_name: "bash",
        args: &args,
        raw: &result,
        exit_code: None,
    };
    let classification = registry.classify(&ctx);
    assert!(
        classification.summary.is_passthrough(),
        "ls output should pass through, not match nix; got {:?}",
        classification.summary
    );
}
