use std::time::Duration;

use async_trait::async_trait;

use crate::error::AdminError;
use crate::types::{Sandbox, SandboxSpecs};

/// Backend-agnostic sandbox lifecycle operations. Backend-specific fields
/// (e.g. `service_fqdn`) are populated where applicable.
#[async_trait]
pub trait AdminClient: Send + Sync {
    /// k8s returns a sandbox that is not yet ready — call `wait_sandbox_ready`
    /// before reading `base_url`. The local backend ignores `specs` and
    /// returns a ready sandbox.
    async fn create_sandbox(&self, name: &str, specs: &SandboxSpecs)
        -> Result<Sandbox, AdminError>;

    async fn delete_sandbox(&self, name: &str) -> Result<(), AdminError>;

    async fn get_sandbox(&self, name: &str) -> Result<Sandbox, AdminError>;

    async fn list_sandboxes(&self) -> Result<Vec<Sandbox>, AdminError>;

    async fn wait_sandbox_ready(
        &self,
        name: &str,
        timeout: Duration,
    ) -> Result<Sandbox, AdminError>;

    /// Local: `<repo_root>/.sandboxes/<name>/workspace`. K8s: persistence
    /// mount path (typically `/workspace`).
    fn mount_path(&self, name: &str) -> String;
}
