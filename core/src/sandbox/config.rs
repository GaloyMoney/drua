use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Selects which [`sandbox::AdminClient`] backend the [`super::Sandboxes`]
/// service uses.
///
/// In `local` mode, sandboxes are spawned as child processes via the given
/// shell command (see [`sandbox::admin_client::LocalSandboxConfig`]). In
/// `k8s` mode, the service talks to the Agent Sandbox controller via the
/// configured namespace + template.
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
        /// When present, sandboxes get a PVC mounted at `mount_path` using
        /// `storage_class`. Without this, sandbox pods use ephemeral storage
        /// and lose workspace state on restart. Per-sandbox disk size comes
        /// from [`SandboxSpecs::disk_size`].
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
