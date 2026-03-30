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
        mcp_upstreams: vec![McpUpstreamConfig {
            name: "honeycomb".to_string(),
            url: "https://mcp.honeycomb.io/mcp".to_string(),
            auth_header: auth_header,
            category: Some("observability".to_string()),
            category_description: Some("Distributed traces, SLOs, and query analysis".to_string()),
        }],
    };
    let toolsets = ToolSets::init(config).await.unwrap();
    let catalog = toolsets.catalog();

    // search_tools
    let all_tools = catalog.search(None, None);
    assert!(!all_tools.is_empty());

    // search with category filter
    let obs_tools = catalog.search(None, Some("observability"));
    assert!(!obs_tools.is_empty());
    let no_tools = catalog.search(None, Some("banking"));
    assert!(no_tools.is_empty());

    // describe_tool
    let first = &all_tools[0];
    let described = catalog.describe(&first.prefixed_name);
    assert!(described.is_some());

    // call_tool round trip
    let result = catalog
        .call("honeycomb_get_workspace_context", None)
        .await
        .unwrap();
    assert!(result.is_error.is_none() || result.is_error == Some(false));
    assert!(!result.content.is_empty());
}
