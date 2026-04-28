use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum SandboxBackendConfig {
    Local {
        sandbox_spawn_cmd: String,
        #[serde(default = "default_local_repo_root")]
        local_repo_root: PathBuf,
    },
    K8s {
        namespace: String,
        template_name: String,
        /// PVC at `mount_path` via `storage_class`; without it pods use
        /// ephemeral storage. Disk size from `SandboxSpecs::disk_size`.
        #[serde(default)]
        storage_class: Option<String>,
        #[serde(default)]
        mount_path: Option<String>,
    },
}

impl Default for SandboxBackendConfig {
    fn default() -> Self {
        Self::Local {
            sandbox_spawn_cmd: "cargo run -q -p sandbox-tool-server --".into(),
            local_repo_root: default_local_repo_root(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SandboxConfig {
    #[serde(default)]
    pub backend: SandboxBackendConfig,
}

fn default_local_repo_root() -> PathBuf {
    PathBuf::from(".")
}
