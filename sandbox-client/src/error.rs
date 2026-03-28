use thiserror::Error;

/// Errors from the Agent Sandbox Kubernetes client.
#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("Kubernetes API error: {0}")]
    Kube(#[from] kube::Error),

    #[error("Sandbox not found: {0}")]
    NotFound(String),

    #[error("Sandbox not ready: {0}")]
    NotReady(String),

    #[error("No running pod found for sandbox: {0}")]
    NoPod(String),

    #[error("Sandbox client not configured")]
    NotConfigured,

    #[error("Timed out waiting for sandbox: {0}")]
    Timeout(String),
}
