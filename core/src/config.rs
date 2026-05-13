use serde::{Deserialize, Serialize};

use crate::agent::AgentsConfig;
use crate::encryption::EncryptionKey;
use crate::github_app::GitHubAppConfig;
use crate::library::LibraryConfig;
use crate::prompt_executor::PromptExecutorConfig;
use crate::sandbox::SandboxConfig;
use crate::toolset::ToolSetsConfig;
use drua_git_proxy::AllowlistConfig;

#[derive(Clone, Debug, Deserialize)]
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
    /// Gates the `bootstrapPersonalProject` GraphQL mutation used by the
    /// TUI on launch. Admins flip this to `false` to disable the
    /// auto-create-on-first-login flow.
    #[serde(default = "default_auto_bootstrap_personal_project")]
    pub auto_bootstrap_personal_project: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            agents: Default::default(),
            prompt_executor: Default::default(),
            toolsets: Default::default(),
            encryption: Default::default(),
            sandbox: Default::default(),
            github_app: Default::default(),
            library: Default::default(),
            git_proxy: Default::default(),
            auto_bootstrap_personal_project: default_auto_bootstrap_personal_project(),
        }
    }
}

fn default_auto_bootstrap_personal_project() -> bool {
    true
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
