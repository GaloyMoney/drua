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

use drua_core::toolset::{
    ClassifierContext, ClassifierRegistry, ConcourseBuildLogClassifier, ConcourseBuildStatus,
    ResultClassifier, ToolResultSummary,
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
    ConcourseBuildLogClassifier
        .classify(&ctx)
        .expect("classifier never errors on valid input")
        .summary
}

#[test]
fn build_610_succeeded_extracts_warnings_and_counts_noise() {
    let raw_size = BUILD_610_SUCCEEDED.len();
    assert!(
        raw_size > 100_000,
        "fixture should be large enough to demonstrate compression (got {raw_size} bytes)"
    );

    let summary = match classify(BUILD_610_SUCCEEDED) {
        ToolResultSummary::ConcourseLogs(s) => s,
        other => panic!("expected ConcourseLogs summary, got {other:?}"),
    };

    // Build #610 succeeded in concourse, but the classifier deliberately
    // doesn't claim `Succeeded` — the log carries no positive pass
    // marker. Ground truth lives in `concourse_get_build_status`; here
    // we only assert that no failure pattern matched.
    assert_eq!(summary.status, ConcourseBuildStatus::NoFailureDetected);
    assert!(
        summary.failures.is_empty(),
        "succeeded build should have no failures"
    );
    assert!(
        summary.errors.is_empty(),
        "succeeded build should have no stray error: lines"
    );

    // The captured fixture has 760 `copying path` lines and 100 cache-pruning
    // lines; treat the exact counts as canon since the fixture is frozen.
    assert_eq!(summary.nix_paths_copied, 760);
    assert_eq!(summary.cache_files_pruned, 100);
    // 16 `checking derivation` + 16 `derivation evaluated to` regex hits.
    assert!(
        summary.derivations_checked >= 16,
        "expected ≥16 derivations, got {}",
        summary.derivations_checked,
    );

    // Five distinct `warning:` lines in the fixture; preserved verbatim.
    assert_eq!(summary.warnings.len(), 5);
    assert!(summary
        .warnings
        .iter()
        .any(|w| w.message.contains("apps.x86_64-linux.bats")));
    assert!(summary
        .warnings
        .iter()
        .any(|w| w.message.contains("incompatible systems")));

    // Task markers: `start` and `setup done` both appear in the fixture.
    assert!(!summary.task_phases.is_empty());

    // Final-tail mirrors the cache-pruning summary lines.
    assert!(summary
        .final_lines
        .iter()
        .any(|l| l.contains("done saving")));

    // Compression: typed summary should be at least 30× smaller than the
    // raw input. Exact threshold is conservative — actual ratio for this
    // fixture is much higher.
    let compression = summary.total_bytes as f64 / summary.kept_bytes.max(1) as f64;
    assert!(
        compression >= 30.0,
        "expected ≥30× compression on succeeded build, got {compression:.1}× \
         ({} → {} bytes)",
        summary.total_bytes,
        summary.kept_bytes,
    );
    eprintln!(
        "build #610: {} bytes raw → {} bytes typed ({:.1}× compression, {} warnings, {} \
         nix paths, {} cache prunes)",
        summary.total_bytes,
        summary.kept_bytes,
        compression,
        summary.warnings.len(),
        summary.nix_paths_copied,
        summary.cache_files_pruned,
    );
}

#[test]
fn build_609_failed_extracts_clippy_rust_diagnostic() {
    let summary = match classify(BUILD_609_FAILED) {
        ToolResultSummary::ConcourseLogs(s) => s,
        other => panic!("expected ConcourseLogs summary, got {other:?}"),
    };

    assert_eq!(summary.status, ConcourseBuildStatus::Failed);
    assert_eq!(
        summary.failures.len(),
        1,
        "exactly one failed derivation in fixture"
    );

    let f = &summary.failures[0];
    assert_eq!(f.attribute, "checks.x86_64-linux.clippy");
    assert!(
        f.drv
            .as_deref()
            .is_some_and(|s| s.contains("drua-clippy-0.1.0.drv")),
        "expected drv path on failure, got {:?}",
        f.drv,
    );
    assert_eq!(
        f.reason.as_deref(),
        Some("builder failed with exit code 101.")
    );

    // The whole point of the classifier — preserve the actual rust error
    // verbatim out of the failure block.
    let log_tail_concat = f.log_tail.join("\n");
    assert!(
        log_tail_concat.contains("error[E0063]: missing fields"),
        "rust diagnostic must survive into the typed summary; got: {log_tail_concat:?}"
    );
    assert!(log_tail_concat.contains("McpUpstreamConfig"));
    assert!(log_tail_concat.contains("auth_mode"));
    assert!(log_tail_concat.contains("internal_only"));

    let compression = summary.total_bytes as f64 / summary.kept_bytes.max(1) as f64;
    eprintln!(
        "build #609: {} bytes raw → {} bytes typed ({:.1}× compression, failed at {}, \
         {} log_tail lines)",
        summary.total_bytes,
        summary.kept_bytes,
        compression,
        f.attribute,
        f.log_tail.len(),
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
    assert_eq!(classification.summary.kind(), "concourse");
}
