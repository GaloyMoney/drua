use serde::Deserialize;

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ToolSetsConfig {
    pub mcp_upstreams: Vec<McpUpstreamConfig>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct McpUpstreamConfig {
    pub name: String,
    pub url: String,
    #[serde(skip)]
    pub auth_header: String,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub category_description: Option<String>,
}
