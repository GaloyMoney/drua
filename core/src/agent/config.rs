use serde::Deserialize;

#[derive(Clone, Debug, Default, Deserialize)]
pub struct AgentConfig {
    #[serde(default)]
    pub sandbox: SandboxClientConfig,
    #[serde(default)]
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
}

#[derive(Clone, Debug, Deserialize)]
pub struct PersistenceConfig {
    pub size: String,
    #[serde(default)]
    pub storage_class: String,
    pub mount_path: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct LightRuntimeConfig {
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_max_turns")]
    pub max_turns: usize,
    #[serde(skip)]
    pub api_key: String,
}

fn default_model() -> String {
    "claude-sonnet-4-20250514".to_string()
}
fn default_max_tokens() -> u32 {
    4096
}
fn default_max_turns() -> usize {
    25
}

impl Default for LightRuntimeConfig {
    fn default() -> Self {
        Self {
            model: default_model(),
            max_tokens: default_max_tokens(),
            max_turns: default_max_turns(),
            api_key: String::new(),
        }
    }
}
