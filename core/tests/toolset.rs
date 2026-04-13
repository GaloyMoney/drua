use galoy_agents_core::toolset::*;

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
        concourse: Default::default(),
        code_assistant: Default::default(),
        mcp_upstreams: vec![McpUpstreamConfig {
            name: "honeycomb".to_string(),
            url: "https://mcp.honeycomb.io/mcp".to_string(),
            auth_header,
            auth_header_name: "authorization".to_string(),
            category: Some("observability".to_string()),
            category_description: Some("Distributed traces, SLOs, and query analysis".to_string()),
            tool_prefix: None,
            allowed_tools: None,
            required_scopes: None,
        }],
    };
    let toolsets = ToolSets::init(config, None).await.unwrap();

    // The unauthenticated `Anonymous` subject only sees built-ins that don't
    // require scopes — search/describe/call_tool. Make sure init succeeded
    // and the registry has the upstream loaded.
    let scopes: Vec<&str> = vec![];
    let visible: Vec<_> = toolsets.top_level_tools(&scopes).collect();
    assert!(visible.iter().any(|t| t.name() == "search_tools"));
}
