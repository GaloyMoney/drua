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

    /// Tear down any persistent storage (PVCs on k8s; nothing on local)
    /// associated with `name`. Called after `delete_sandbox` during
    /// project-cascade cleanup so we don't leak paid storage when the
    /// owning project goes away. Idempotent: missing storage is a no-op.
    async fn delete_pvcs(&self, name: &str) -> Result<(), AdminError>;

    /// Distinct `sandbox-name` label values across every PVC this
    /// backend owns (k8s: `managed-by=drua`). Feeds the PVC janitor,
    /// which deletes any owner with no live DB row. Local backend has
    /// no persistent volumes → empty.
    async fn list_pvc_owners(&self) -> Result<Vec<String>, AdminError>;

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
