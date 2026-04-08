use serde::Deserialize;

#[derive(Clone, Debug, Default, Deserialize)]
pub struct AgentConfig {
    #[serde(default)]
    pub sandbox: SandboxClientConfig,
    #[serde(skip)]
    pub light: LightRuntimeConfig,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct SandboxClientConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub namespace: String,
    #[serde(default)]
    pub template_name: String,
    #[serde(default)]
    pub persistence: Option<PersistenceConfig>,
    /// MCP gateway URL that sandbox agents connect to (e.g. "http://galoy-agents:4200/mcp").
    #[serde(default)]
    pub mcp_gateway_url: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PersistenceConfig {
    pub size: String,
    #[serde(default)]
    pub storage_class: String,
    pub mount_path: String,
}

#[derive(Clone, Debug, Default)]
pub struct LightRuntimeConfig {
    pub api_key: String,
}
