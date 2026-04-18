use serde::{Deserialize, Serialize};

/// Configuration for the Podman sandbox backend.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PodmanSandboxConfig {
    /// OCI image reference for the sandbox container.
    #[serde(default = "default_image")]
    pub image: String,

    /// Path to the podman binary.
    #[serde(default = "default_podman_bin")]
    pub podman_bin: String,
}

fn default_image() -> String {
    "localhost/sandbox:latest".into()
}

fn default_podman_bin() -> String {
    "podman".into()
}
