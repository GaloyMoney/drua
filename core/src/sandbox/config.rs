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
#[serde(tag = "backend", rename_all = "snake_case")]
pub enum SandboxBackendConfig {
    Local {
        sandbox_spawn_cmd: String,
    },
    K8s {
        namespace: String,
        template_name: String,
    },
}

impl Default for SandboxBackendConfig {
    fn default() -> Self {
        Self::Local {
            sandbox_spawn_cmd: "cargo run -q -p sandbox-tool-server --".into(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SandboxConfig {
    #[serde(default)]
    pub backend: SandboxBackendConfig,
    /// For local mode: parent of the `.sandboxes/` directory. Defaults to `.`.
    #[serde(default = "default_local_repo_root")]
    pub local_repo_root: PathBuf,
}

fn default_local_repo_root() -> PathBuf {
    PathBuf::from(".")
}
