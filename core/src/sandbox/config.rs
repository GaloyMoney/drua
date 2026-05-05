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
        /// Per-sandbox `/nix` PVC. Required for workloads whose nix
        /// closure exceeds the sandbox container's writable layer
        /// (e.g. `nix develop` on lana-bank pulls > 20 GiB). Seeded
        /// from the image's baked-in store on first boot via an init
        /// container — without seeding the empty mount shadows every
        /// binary in the sandbox image.
        #[serde(default)]
        nix_store: Option<NixStorePersistenceConfig>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct NixStorePersistenceConfig {
    pub storage_class: String,
    /// e.g. `"50Gi"`. Sized for the full nix closure of whatever flake
    /// the sandbox builds plus headroom for cargo build artifacts.
    pub size: String,
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
