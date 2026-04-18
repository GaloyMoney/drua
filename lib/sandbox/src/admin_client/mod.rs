//! Backend-agnostic admin clients for managing sandbox lifecycle.

pub mod k8s;
pub mod local;
pub mod podman;
mod r#trait;

pub use k8s::K8sAdminClient;
pub use local::{LocalAdminClient, LocalSandboxConfig};
pub use podman::{PodmanAdminClient, PodmanSandboxConfig};
pub use r#trait::AdminClient;
