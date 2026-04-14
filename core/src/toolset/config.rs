use serde::Deserialize;

use crate::primitives::AuthScope;

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ToolSetsConfig {
    #[serde(default)]
    pub mcp_upstreams: Vec<McpUpstreamConfig>,
    #[serde(default)]
    pub concourse: ConcourseToolSetConfig,
    #[serde(default)]
    pub code_assistant: CodeAssistantToolSetConfig,
    #[serde(default)]
    pub sandbox: SandboxToolConfig,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct SandboxToolConfig {
    /// Base URL of the sandbox tool server (e.g. "http://sandbox:3000").
    /// When non-empty, `bash` and `text_editor` top-level tools are registered.
    #[serde(default)]
    pub url: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct CodeAssistantToolSetConfig {
    #[serde(default)]
    pub db_path: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct McpUpstreamConfig {
    pub name: String,
    pub url: String,
    #[serde(skip)]
    pub auth_header: String,
    #[serde(default = "default_auth_header_name")]
    pub auth_header_name: String,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub category_description: Option<String>,
    /// Optional prefix for tool names (defaults to `name` if unset).
    #[serde(default)]
    pub tool_prefix: Option<String>,
    /// Optional whitelist of tool names to expose from this upstream.
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
    /// Scopes required to access this upstream. Empty means unrestricted.
    #[serde(default)]
    pub required_scopes: Option<Vec<AuthScope>>,
}

fn default_auth_header_name() -> String {
    "authorization".to_string()
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ConcourseToolSetConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub team: String,
    #[serde(skip)]
    pub username: String,
    #[serde(skip)]
    pub password: String,
}
