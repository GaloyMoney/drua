use std::time::Duration;

use async_trait::async_trait;

use crate::error::AdminError;
use crate::types::{Sandbox, SandboxSpecs};

/// Backend-agnostic operations for managing sandbox lifecycle.
///
/// Implementations return the unified [`Sandbox`] shape; backend-specific
/// fields (e.g. `service_fqdn`) are populated where applicable and left
/// `None` otherwise.
#[async_trait]
pub trait AdminClient: Send + Sync {
    /// Create a new sandbox with the given resource specs. The returned
    /// [`Sandbox`] may not yet be ready (k8s in particular needs a follow-up
    /// [`Self::wait_sandbox_ready`] before its `base_url` is populated).
    /// The local backend returns a ready sandbox and ignores `specs`.
    async fn create_sandbox(
        &self,
        name: &str,
        specs: &SandboxSpecs,
    ) -> Result<Sandbox, AdminError>;

    /// Delete a sandbox by name. Returns [`AdminError::NotFound`] if no
    /// such sandbox is tracked / exists.
    async fn delete_sandbox(&self, name: &str) -> Result<(), AdminError>;

    /// Look up a single sandbox by name.
    async fn get_sandbox(&self, name: &str) -> Result<Sandbox, AdminError>;

    /// List all sandboxes managed by this client.
    async fn list_sandboxes(&self) -> Result<Vec<Sandbox>, AdminError>;

    /// Block until the sandbox becomes ready (`base_url` populated) or
    /// `timeout` elapses.
    async fn wait_sandbox_ready(
        &self,
        name: &str,
        timeout: Duration,
    ) -> Result<Sandbox, AdminError>;
}
