use serde::{Deserialize, Serialize};

use crate::agent::AgentsConfig;
use crate::changeset::ChangesetConfig;
use crate::encryption::EncryptionKey;
use crate::github_app::GitHubAppConfig;
use crate::library::LibraryConfig;
use crate::prompt_executor::PromptExecutorConfig;
use crate::sandbox::SandboxConfig;
use crate::toolset::ToolSetsConfig;
use drua_git_proxy::AllowlistConfig;

pub use drua_tunnel::{
    default_tunnel_expires_after_secs, default_tunnel_heartbeat_secs,
    default_tunnel_reaper_interval_secs, TunnelRuntimeConfig,
};

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
    #[serde(default)]
    pub changeset: ChangesetConfig,
    /// YAML-driven allow-list for the smart-HTTP git proxy. Restart
    /// the server to apply edits (no live reload — memo `019dfebc` §7.2).
    #[serde(default)]
    pub git_proxy: GitProxyAppConfig,
    #[serde(default)]
    pub tunnel_runtime: TunnelRuntimeConfig,
    /// Tool-output elision thresholds (the walker in `drua-tool-caching`).
    /// Absent config reproduces the crate's own defaults.
    #[serde(default)]
    pub tool_caching: drua_tool_caching::ToolCachingConfig,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GitProxyAppConfig {
    #[serde(default)]
    pub allowlist: AllowlistConfig,
    /// Per-project bare mirrors live at `<mirror_root>/<project_id>/<owner>/<repo>.git`.
    /// Defaults to `./.git-proxy-mirrors` (good for local dev). Production
    /// should set this to a PVC path.
    #[serde(default)]
    pub mirror_root: Option<String>,
    /// Pulls within this window reuse the existing mirror without
    /// re-fetching from upstream. Default 300s (memo §7.1).
    #[serde(default = "default_mirror_ttl_seconds")]
    pub mirror_ttl_seconds: u64,
}

fn default_mirror_ttl_seconds() -> u64 {
    300
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_config_without_tool_caching_block_yields_crate_defaults() {
        let config: AppConfig = serde_yaml::from_str("{}").expect("empty config parses");
        let default_tc = drua_tool_caching::ToolCachingConfig::default();
        assert_eq!(
            config.tool_caching.generic_threshold_bytes,
            default_tc.generic_threshold_bytes
        );
        assert_eq!(
            config.tool_caching.min_hidden_bytes,
            default_tc.min_hidden_bytes
        );
        assert!(config.tool_caching.per_tool.is_empty());
    }

    #[test]
    fn app_config_parses_tool_caching_block_with_per_tool_overrides() {
        let yaml = r#"
tool_caching:
  min_hidden_bytes: 4096
  per_tool:
    library_get_files:
      generic_threshold_bytes: 65536
"#;
        let config: AppConfig = serde_yaml::from_str(yaml).expect("valid tool_caching block");
        assert_eq!(config.tool_caching.min_hidden_bytes, 4096);
        // Untouched fields keep the crate default.
        assert_eq!(config.tool_caching.generic_threshold_bytes, 8192);
        let over = config
            .tool_caching
            .per_tool
            .get("library_get_files")
            .expect("per-tool override present");
        assert_eq!(over.generic_threshold_bytes, Some(65536));
    }
}
