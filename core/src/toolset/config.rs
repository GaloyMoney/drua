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
    /// How this upstream is dialed. `Http` (default) uses a static auth
    /// header set from an env var. `RemoteProxy` mints a short-lived RS256
    /// JWT on init via the shared `McpJwtSigner`, with `audience` set to
    /// the deployment's public hostname so remote Envoys can validate it.
    #[serde(default)]
    pub kind: McpUpstreamKind,
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

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpUpstreamKind {
    /// Standard HTTP upstream authenticated via a static header.
    #[default]
    Http,
    /// Remote MCP proxy running in a target deployment (e.g. the
    /// galoy-agents-proxy sidecar). Outbound calls are authenticated with
    /// a JWT signed by galoy-agents' shared signing key; the remote Envoy
    /// validates via `/.well-known/jwks.json`.
    RemoteProxy {
        /// `aud` claim for the minted JWT. Must match the remote Envoy's
        /// configured audience (typically the public hostname).
        audience: String,
    },
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
