use thiserror::Error;

/// Errors from the Agent Sandbox Kubernetes client.
#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("Kubernetes API error: {0}")]
    Kube(#[from] kube::Error),

    #[error("Sandbox not found: {0}")]
    NotFound(String),

    #[error("Sandbox client not configured")]
    NotConfigured,
}
