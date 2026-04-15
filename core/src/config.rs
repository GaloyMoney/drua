use serde::Deserialize;

use crate::agent::AgentsConfig;
use crate::encryption::EncryptionKey;
use crate::github_app::GitHubAppConfig;
use crate::mcp_jwt::McpJwtConfig;
use crate::prompt_executor::PromptExecutorConfig;
use crate::sandbox::SandboxConfig;
use crate::toolset::ToolSetsConfig;

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
    /// Optional MCP JWT signer config. Required when any `RemoteProxy`
    /// upstream is configured; those upstreams sign outbound calls with
    /// this key and expose the matching public key at `/.well-known/jwks.json`.
    #[serde(default)]
    pub mcp_jwt: Option<McpJwtConfig>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct EncryptionConfig {
    /// Hex-encoded 32-byte encryption key for workspace secrets.
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
