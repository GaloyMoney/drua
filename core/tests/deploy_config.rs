const PROD_VALUES: &str = include_str!("../../ci/deploy/drua/prod-values.yml.tmpl");

#[test]
fn prod_github_actions_upstream_uses_readonly_actions_endpoint() {
    let block = upstream_block(PROD_VALUES, "github_actions");

    assert!(
        block.contains("url: https://api.githubcopilot.com/mcp/x/actions/readonly"),
        "github_actions must point at the Actions readonly MCP endpoint, not the generic GitHub catalog:\n{block}"
    );
    assert!(block.contains("toolPrefix: github_actions"));
    assert!(block.contains("authMode: github_app"));
    assert!(block.contains("category: ci"));
}

#[test]
fn prod_github_actions_upstream_exposes_only_read_tools() {
    let block = upstream_block(PROD_VALUES, "github_actions");

    for tool in ["actions_list", "actions_get", "get_job_logs"] {
        assert!(
            block.contains(&format!("- {tool}")),
            "github_actions upstream is missing expected read-only tool {tool}:\n{block}"
        );
    }

    for forbidden in ["rerun", "cancel", "delete", "approve"] {
        assert!(
            !block.contains(forbidden),
            "github_actions upstream should not expose CI mutation capability containing {forbidden}:\n{block}"
        );
    }
}

fn upstream_block<'a>(values: &'a str, name: &str) -> &'a str {
    let marker = format!("      - name: {name}");
    let start = values
        .find(&marker)
        .unwrap_or_else(|| panic!("missing upstream block marker {marker}"));
    let rest = &values[start..];
    let end = rest
        .find("\n      - name: ")
        .filter(|idx| *idx > 0)
        .unwrap_or(rest.len());
    &rest[..end]
}
