//! Drives [`ConcourseBuildLogClassifier`] against captured fixtures from
//! ci.galoy.io's `galoy-agents-bin` pipeline (`check-code` job, builds #610
//! and #609).
//!
//! Architecture under test:
//! - Concourse classifier preprocesses (strips ANSI + timestamps) and
//!   delegates to the walker, producing a `Typed` summary.
//! - `typed_kind = "concourse_logs"` matches the upstream tool's
//!   identity for filterable PG queries.
//! - `body = { logs: String }` matches the upstream tool's
//!   declared `output_schema`.

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

/// Helper: extract the `logs` string from a `Typed` summary.
fn typed_logs(summary: &ToolResultSummary) -> &str {
    match summary {
        ToolResultSummary::Typed { typed_kind, body } => {
            assert_eq!(typed_kind, "concourse_logs");
            body.get("logs")
                .and_then(|v| v.as_str())
                .expect("body.logs is a string")
        }
        other => panic!("expected Typed summary, got {other:?}"),
    }
}

#[test]
fn build_610_succeeded_compacts_copy_runs_inline() {
    let raw_size = BUILD_610_SUCCEEDED.len();
    assert!(raw_size > 100_000);

    let summary = classify(BUILD_610_SUCCEEDED);
    let logs = typed_logs(&summary);

    // 700+ `copying path` lines collapsed into <nix-copy>.
    assert!(logs.contains("<nix-copy"));
    assert!(logs.contains("</nix-copy>"));
    assert_eq!(logs.matches("copying path '/nix/store/").count(), 0);

    // Total size is bounded by the BulkElide terminal pass.
    assert!(
        logs.len() <= 16 * 1024,
        "post-chain logs len {} exceeds BulkElide cap",
        logs.len()
    );

    // Warning lines survive as ordinary text inside `logs`.
    assert!(logs.contains("warning:"));

    eprintln!(
        "build #610: {} bytes raw → {} bytes kept",
        BUILD_610_SUCCEEDED.len(),
        logs.len(),
    );
}

#[test]
fn build_609_failed_compacts_under_bulk_elide_cap() {
    let summary = classify(BUILD_609_FAILED);
    let logs = typed_logs(&summary);

    // The chain compresses #609 well under the BulkElide cap.
    // Failure header + indented `> ` log_tail stay inline as
    // ordinary text — no marker, no special handling.
    assert!(logs.len() <= 16 * 1024);
    assert!(logs.contains("error: failed to build attribute"));
    assert!(logs.contains("drua-clippy"));
    assert!(logs.contains("error[E0063]: missing fields"));
    assert!(logs.contains("McpUpstreamConfig"));

    eprintln!(
        "build #609: {} bytes raw → {} bytes kept",
        BUILD_609_FAILED.len(),
        logs.len(),
    );
}

#[test]
fn registry_routes_concourse_to_typed_classifier_with_concourse_logs_kind() {
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
    // PG `kind` column gets the typed_kind, not the serde tag.
    assert_eq!(classification.summary.kind(), "concourse_logs");
}

/// Schema-faithfulness check: the body conforms to the upstream
/// MCP tool's `output_schema` (`{logs: String}`).
#[test]
fn typed_body_conforms_to_concourse_output_schema() {
    let summary = classify(BUILD_610_SUCCEEDED);
    let body = match summary {
        ToolResultSummary::Typed { body, .. } => body,
        other => panic!("expected Typed, got {other:?}"),
    };
    let obj = body.as_object().expect("body is an object");
    assert_eq!(obj.len(), 1, "schema is `{{logs: String}}`; got: {obj:?}");
    assert!(
        obj.get("logs").map(|v| v.is_string()).unwrap_or(false),
        "body.logs must be a string"
    );
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
    let summary = ConcourseBuildLogClassifier::new(chain)
        .classify(&ClassifierContext {
            tool_name: "concourse_get_build_logs",
            args: &args,
            raw: &result,
            exit_code: None,
        })
        .expect("classify")
        .summary;

    let logs = typed_logs(&summary);
    assert!(
        logs.len() <= 16 * 1024,
        "post-chain logs len {} exceeds BulkElide cap",
        logs.len()
    );
    assert!(logs.contains("<bulk-elided"));
    // Simple tail-keep — head is dropped, tail survives.
    assert!(logs.contains("tail line"));
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
