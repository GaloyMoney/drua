use serde::Deserialize;

use crate::agent::AgentConfig;
use crate::encryption::EncryptionKey;
use crate::toolset::ToolSetsConfig;

#[derive(Clone, Debug, Default, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub agents: AgentConfig,
    #[serde(default)]
    pub toolsets: ToolSetsConfig,
    #[serde(default)]
    pub encryption: EncryptionConfig,
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
