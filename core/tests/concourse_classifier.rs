//! Drives [`ConcourseBuildLogClassifier`] against captured fixtures from
//! ci.galoy.io's `galoy-agents-bin` pipeline (`check-code` job, builds #610
//! and #609). Fixtures live under `core/tests/fixtures/concourse/`.
//!
//! Architecture under test:
//! - Concourse classifier preprocesses (strips ANSI + timestamps).
//! - Output is a schema-faithful `{ logs: String }` summary.
//! - The chain compacts copy-runs, drv-lists, cache-removal, and
//!   git-clone progress runs into XML markers; the terminal
//!   `BulkElide` pass bounds total size.

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
    assert!(raw_size > 100_000);

    let summary = match classify(BUILD_610_SUCCEEDED) {
        ToolResultSummary::ConcourseLogs(s) => s,
        other => panic!("expected ConcourseLogs summary, got {other:?}"),
    };

    // Schema-faithful: the only field is `logs`.
    let v = serde_json::to_value(&summary).expect("serialise");
    let obj = v.as_object().expect("summary is an object");
    assert_eq!(obj.len(), 1);
    assert!(obj.contains_key("logs"));

    // 700+ `copying path` lines collapsed into <nix-copy>.
    assert!(summary.logs.contains("<nix-copy"));
    assert!(summary.logs.contains("</nix-copy>"));
    assert_eq!(summary.logs.matches("copying path '/nix/store/").count(), 0);

    // Total size is bounded by the BulkElide terminal pass.
    assert!(
        summary.logs.len() <= 16 * 1024,
        "post-chain logs len {} exceeds BulkElide cap",
        summary.logs.len()
    );

    // Warning lines survive as ordinary text inside `logs`.
    assert!(summary.logs.contains("warning:"));

    eprintln!(
        "build #610: {} bytes raw → {} bytes kept",
        BUILD_610_SUCCEEDED.len(),
        summary.logs.len(),
    );
}

#[test]
fn build_609_failed_compacts_under_bulk_elide_cap() {
    let summary = match classify(BUILD_609_FAILED) {
        ToolResultSummary::ConcourseLogs(s) => s,
        other => panic!("expected ConcourseLogs summary, got {other:?}"),
    };

    // The chain compresses #609 well under the BulkElide cap.
    // Failure header + indented `> ` log_tail stay inline as
    // ordinary text — no marker, no special handling.
    assert!(summary.logs.len() <= 16 * 1024);
    assert!(summary.logs.contains("error: failed to build attribute"));
    assert!(summary.logs.contains("drua-clippy"));
    assert!(summary.logs.contains("error[E0063]: missing fields"));
    assert!(summary.logs.contains("McpUpstreamConfig"));

    eprintln!(
        "build #609: {} bytes raw → {} bytes kept",
        BUILD_609_FAILED.len(),
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

/// Bash-shape result with nix-build chatter: walker descends to the
/// string leaf, runs the chain, returns Value::String with markers
/// substituted in place. Schema-faithful — kept stays a String.
#[test]
fn nix_summarizers_fire_on_bash_output_via_walker() {
    let mut nix_output = String::from("preparing to build /nix/store/aaaa-foo.drv\n");
    nix_output.push_str("building '/nix/store/aaaa-foo.drv'\n");
    for i in 0..30 {
        nix_output.push_str(&format!(
            "copying path '/nix/store/bbb{i:02}-bar' from 'https://cache.nixos.org/'\n"
        ));
    }
    nix_output.push_str("error: builder for '/nix/store/aaaa-foo.drv' failed with exit code 1\n");
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
    assert!(s.contains("<nix-copy"));
    assert!(s.contains("error: builder for"));
    assert!(s.contains("some compiler error"));
    assert!(!s.contains("copying path '/nix/store/"));
}

/// Pathological log: hundreds of KB of unstructured chatter that no
/// structured pass matches. The terminal `BulkElide` pass must still
/// bring the post-chain log under its byte budget.
#[test]
fn bulk_elide_bounds_unstructured_log_size() {
    let mut raw = String::from("[03:00:00] === with-nix-cache: start ===\n");
    for i in 0..5_000 {
        raw.push_str(&format!(
            "[03:00:01] some-tool: long unstructured diagnostic line #{i:05} with arbitrary text\n"
        ));
    }
    raw.push_str("[03:99:99] tail line\n");

    let mut result = CallToolResult::success(vec![Content::text(raw.clone())]);
    result.structured_content = Some(serde_json::json!({"logs": raw.clone()}));
    let args = serde_json::json!({"build_id": 1u64});
    let chain = Arc::new(default_summarizer_chain());
    let summary = match ConcourseBuildLogClassifier::new(chain)
        .classify(&ClassifierContext {
            tool_name: "concourse_get_build_logs",
            args: &args,
            raw: &result,
            exit_code: None,
        })
        .expect("classify")
        .summary
    {
        ToolResultSummary::ConcourseLogs(s) => s,
        other => panic!("unexpected summary shape: {other:?}"),
    };

    assert!(
        summary.logs.len() <= 16 * 1024,
        "post-chain logs len {} exceeds BulkElide cap",
        summary.logs.len()
    );
    assert!(summary.logs.contains("<bulk-elided"));
    // Simple tail-keep — head is dropped, tail survives.
    assert!(summary.logs.contains("tail line"));
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
    assert!(classification.summary.is_passthrough());
}
