use serde::Deserialize;

#[derive(Clone, Debug, Default, Deserialize)]
pub struct AgentConfig {
    #[serde(default)]
    pub sandbox: SandboxClientConfig,
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
}

#[derive(Clone, Debug, Deserialize)]
pub struct PersistenceConfig {
    pub size: String,
    #[serde(default)]
    pub storage_class: String,
    pub mount_path: String,
}
