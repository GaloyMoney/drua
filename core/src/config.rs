use serde::{Deserialize, Serialize};

use crate::agent::AgentsConfig;
use crate::encryption::EncryptionKey;
use crate::github_app::GitHubAppConfig;
use crate::library::LibraryConfig;
use crate::prompt_executor::PromptExecutorConfig;
use crate::sandbox::SandboxConfig;
use crate::toolset::ToolSetsConfig;
use drua_git_proxy::AllowlistConfig;

#[derive(Clone, Debug, Default, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub agents: AgentsConfig,
    #[serde(default)]
    pub prompt_executor: PromptExecutorConfig,
    #[serde(default)]
    pub toolsets: ToolSetsConfig,
    #[serde(default)]
    pub encryption: EncryptionConfig,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    /// Optional GitHub App config for token auto-provisioning.
    /// When set, sandbox agents receive a `github-token` file secret.
    #[serde(default)]
    pub github_app: Option<GitHubAppConfig>,
    #[serde(default)]
    pub library: LibraryConfig,
    /// YAML-driven allow-list for the smart-HTTP git proxy. Restart
    /// the server to apply edits (no live reload — memo `019dfebc` §7.2).
    #[serde(default)]
    pub git_proxy: GitProxyAppConfig,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GitProxyAppConfig {
    #[serde(default)]
    pub allowlist: AllowlistConfig,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct EncryptionConfig {
    /// Hex-encoded 32-byte encryption key for project secrets.
    /// If not provided, a zeroed key is used (development only).
    #[serde(default)]
    pub secret_key_hex: Option<String>,
}

impl EncryptionConfig {
    pub fn encryption_key(&self) -> EncryptionKey {
        match &self.secret_key_hex {
            Some(hex) => {
                let bytes = hex::decode(hex).expect("ENCRYPTION_SECRET_KEY_HEX must be valid hex");
                let mut key = [0u8; 32];
                key.copy_from_slice(&bytes);
                EncryptionKey::new(key)
            }
            None => EncryptionKey::default(),
        }
    }
}
