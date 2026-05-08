use drua_core::primitives::AuthSubject;
use drua_core::toolset::*;

#[tokio::test]
async fn init_toolsets() {
    let auth_header = match std::env::var("HONEYCOMB_AUTH_HEADER") {
        Ok(val) => val,
        Err(_) => {
            eprintln!("HONEYCOMB_AUTH_HEADER not set, skipping");
            return;
        }
    };

    let config = ToolSetsConfig {
        mcp_upstreams: vec![McpUpstreamConfig {
            name: "honeycomb".to_string(),
            url: "https://mcp.honeycomb.io/mcp".to_string(),
            auth_header,
            auth_header_name: "authorization".to_string(),
            auth_required: true,
            category: Some("observability".to_string()),
            category_description: Some("Distributed traces, SLOs, and query analysis".to_string()),
            tool_prefix: None,
            allowed_tools: None,
            required_scopes: None,
        }],
        ..Default::default()
    };
    let toolsets = ToolSets::init(config, None, None).await.unwrap();

    // Anonymous subject still sees the unrestricted builtins — the new
    // trait defaults `is_visible` to `true`. Make sure init succeeded and
    // the registry exposes the catalog meta-tools.
    let subject = AuthSubject::Anonymous;
    let visible: Vec<_> = toolsets.top_level_tool_arcs(&subject).collect();
    assert!(visible.iter().any(|t| t.name() == "search_tools"));
}
