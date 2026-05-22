use std::path::PathBuf;

use drua_server::config::{Config, EnvSecrets};

const REVIEW_REPLY_TOOL: &str = "add_reply_to_pull_request_comment";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("server crate has repo parent")
        .to_path_buf()
}

fn empty_secrets() -> EnvSecrets {
    EnvSecrets {
        pg_con: String::new(),
        github_client_secret: String::new(),
        github_allowed_teams: Vec::new(),
        anthropic_api_key: String::new(),
        openai_api_key: String::new(),
    }
}

#[test]
fn local_config_exposes_internal_github_review_comment_reply_tool() {
    let config =
        Config::try_new(repo_root().join("drua.yml"), empty_secrets(), &[]).expect("load drua.yml");

    let upstream = config
        .toolsets
        .mcp_upstreams
        .iter()
        .find(|u| u.name == "github_pull_requests")
        .expect("github_pull_requests upstream");

    assert!(upstream.internal_only);
    assert_eq!(upstream.tool_prefix.as_deref(), Some("github_pr"));
    assert!(
        upstream
            .allowed_tools
            .as_ref()
            .is_some_and(|tools| tools.iter().any(|tool| tool == REVIEW_REPLY_TOOL)),
        "{REVIEW_REPLY_TOOL} should be allowlisted for workflow agents"
    );
}

#[test]
fn prod_values_allowlist_includes_github_review_comment_reply_tool() {
    let prod_values =
        std::fs::read_to_string(repo_root().join("ci/deploy/drua/prod-values.yml.tmpl"))
            .expect("read prod values template");

    assert!(
        prod_values.contains(REVIEW_REPLY_TOOL),
        "{REVIEW_REPLY_TOOL} should be present in prod MCP upstream allowlist"
    );
}
