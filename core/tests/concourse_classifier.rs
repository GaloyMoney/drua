//! Drives [`ConcourseBuildLogClassifier`] against captured fixtures from
//! ci.galoy.io's `galoy-agents-bin` pipeline (`check-code` job, builds #610
//! and #609). Fixtures live under `core/tests/fixtures/concourse/`.
//!
//! Architecture under test:
//! - Concourse classifier preprocesses (strips ANSI + timestamps,
//!   captures envelope metadata).
//! - Walker descends into the augmented value's `logs` field and
//!   runs the `StringSummarizerChain` (Nix passes) over the
//!   timestamped-stripped log text.
//! - The compacted text — with `<nix-copy>`, `<nix-drv-list>`, and
//!   `<nix-failure>` XML markers replacing the bulk of the noise —
//!   ends up in `summary.logs`.

use rmcp::model::{CallToolResult, Content};

use std::sync::Arc;

use drua_core::toolset::{
    default_summarizer_chain, ClassifierContext, ClassifierRegistry, ConcourseBuildLogClassifier,
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
    let chain = Arc::new(default_summarizer_chain());
    ConcourseBuildLogClassifier::new(chain)
        .classify(&ctx)
        .expect("classifier never errors on valid input")
        .summary
}

#[test]
fn build_610_succeeded_compacts_copy_runs_inline() {
    let raw_size = BUILD_610_SUCCEEDED.len();
    assert!(
        raw_size > 100_000,
        "fixture should be large enough to demonstrate compression (got {raw_size} bytes)"
    );

    let summary = match classify(BUILD_610_SUCCEEDED) {
        ToolResultSummary::ConcourseLogs(s) => s,
        other => panic!("expected ConcourseLogs summary, got {other:?}"),
    };

    // Status is no longer guessed from log content; only the
    // upstream API call can provide ground truth.
    assert!(summary.status.is_none());

    // Five distinct `warning:` lines in the fixture; preserved with
    // line numbers so an agent can `tool_output_fetch(range=…)`
    // around any of them.
    assert_eq!(summary.warnings.len(), 5);
    for w in &summary.warnings {
        assert!(w.line_number > 0, "every warning carries a line number");
    }
    assert!(summary
        .warnings
        .iter()
        .any(|w| w.message.contains("apps.x86_64-linux.bats")));
    assert!(!summary.task_phases.is_empty());
    assert!(summary.first_timestamp.is_some());
    assert!(summary.last_timestamp.is_some());
    assert!(summary
        .final_lines
        .iter()
        .any(|l| l.contains("done saving")));

    // The 700+ `copying path` lines should have collapsed into one
    // or more `<nix-copy>` markers in `summary.logs`.
    assert!(
        summary.logs.contains("<nix-copy"),
        "expected a <nix-copy> marker in compacted logs"
    );
    assert!(summary.logs.contains("</nix-copy>"));
    let copying_lines = summary.logs.matches("copying path '/nix/store/").count();
    assert!(
        copying_lines == 0,
        "all copy-path lines should be collapsed; {copying_lines} survived"
    );

    eprintln!(
        "build #610: {} bytes raw → {} bytes kept ({} warnings, logs len {})",
        summary.total_bytes,
        summary.kept_bytes,
        summary.warnings.len(),
        summary.logs.len(),
    );
}

#[test]
fn build_609_failed_keeps_clippy_diagnostic_inline_in_failure_marker() {
    let summary = match classify(BUILD_609_FAILED) {
        ToolResultSummary::ConcourseLogs(s) => s,
        other => panic!("expected ConcourseLogs summary, got {other:?}"),
    };

    // The classifier doesn't guess; status stays None.
    assert!(summary.status.is_none());

    // The failure block lands inline in `summary.logs` as a
    // `<nix-failure>` marker carrying `drv=…` plus the `> ` log_tail
    // body. The clippy E0063 diagnostic must survive into that body.
    assert!(
        summary.logs.contains("<nix-failure"),
        "expected a <nix-failure> marker; got logs head:\n{}",
        &summary.logs[..summary.logs.len().min(2_000)]
    );
    assert!(summary.logs.contains("drua-clippy-0.1.0.drv"));
    assert!(summary.logs.contains("error[E0063]: missing fields"));
    assert!(summary.logs.contains("McpUpstreamConfig"));
    assert!(summary.logs.contains("auth_mode"));
    assert!(summary.logs.contains("internal_only"));

    eprintln!(
        "build #609: {} bytes raw → {} bytes kept (logs len {})",
        summary.total_bytes,
        summary.kept_bytes,
        summary.logs.len(),
    );
}

#[test]
fn registry_routes_concourse_to_typed_classifier() {
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
/// build lands in `StructuredElision { kept }` where `kept` is a
/// `Value::String` (schema-faithful) with `<nix-copy>` and
/// `<nix-failure>` markers substituted in place — no JSON
/// substitution, just inline string compaction.
#[test]
fn nix_summarizers_fire_on_bash_output_via_walker() {
    let mut nix_output = String::from("preparing to build /nix/store/aaaa-foo.drv\n");
    nix_output.push_str("building '/nix/store/aaaa-foo.drv'\n");
    for i in 0..30 {
        nix_output.push_str(&format!(
            "copying path '/nix/store/bbb{i:02}-bar' from 'https://cache.nixos.org/'\n"
        ));
    }
    nix_output.push_str(
        "error: builder for '/nix/store/aaaa-foo.drv' failed with exit code 1; last 10 log lines:\n",
    );
    nix_output.push_str("       > some compiler error\n");
    nix_output.push_str("       > another diagnostic line\n");

    let registry = ClassifierRegistry::with_default();
    let result = CallToolResult::success(vec![Content::text(nix_output.clone())]);
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
        ToolResultSummary::Passthrough { value } => value,
        other => panic!("unexpected summary shape: {other:?}"),
    };
    let s = kept.as_str().expect("kept must remain Value::String");
    assert!(s.contains("<nix-copy"), "got: {s}");
    assert!(s.contains("</nix-copy>"), "got: {s}");
    assert!(s.contains("<nix-failure"), "got: {s}");
    assert!(s.contains("some compiler error"), "got: {s}");
    // The 30 `copying path` lines should have collapsed.
    assert!(
        !s.contains("copying path '/nix/store/"),
        "no raw copy lines should survive; got: {s}"
    );
}

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
        "ls output should pass through; got {:?}",
        classification.summary
    );
}
