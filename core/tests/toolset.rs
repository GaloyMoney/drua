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
        memory: Default::default(),
        mcp_upstreams: vec![McpUpstreamConfig {
            name: "honeycomb".to_string(),
            url: "https://mcp.honeycomb.io/mcp".to_string(),
            auth_header,
            auth_header_name: "authorization".to_string(),
            category: Some("observability".to_string()),
            category_description: Some("Distributed traces, SLOs, and query analysis".to_string()),
            tool_prefix: None,
            allowed_tools: None,
        }],
    };
    let toolsets = ToolSets::init(config, None, None, None).await.unwrap();
    let catalog = toolsets.catalog();

    // search_tools
    let all_tools = catalog.search(None, None).await;
    assert!(!all_tools.is_empty());

    // search with category filter
    let obs_tools = catalog.search(None, Some("observability")).await;
    assert!(!obs_tools.is_empty());
    let no_tools = catalog.search(None, Some("banking")).await;
    assert!(no_tools.is_empty());

    // describe_tool
    let first = &all_tools[0];
    let described = catalog.describe(&first.prefixed_name).await;
    assert!(described.is_some());

    // call_tool round trip
    let result = catalog
        .call("honeycomb_get_workspace_context", None)
        .await
        .unwrap();
    assert!(result.is_error.is_none() || result.is_error == Some(false));
    assert!(!result.content.is_empty());
}
