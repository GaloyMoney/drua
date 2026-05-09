use thiserror::Error;

/// Errors from any [`crate::AdminClient`] backend.
#[derive(Debug, Error)]
pub enum AdminError {
    #[error("Sandbox not found: {0}")]
    NotFound(String),

    #[error("Sandbox already exists: {0}")]
    AlreadyExists(String),

    #[error("Timed out waiting for sandbox: {0}")]
    Timeout(String),

    #[error("No running pod found for sandbox: {0}")]
    NoPod(String),

    #[error("Kubernetes API error: {0}")]
    Kube(#[from] kube::Error),

    #[error("Failed to allocate port: {0}")]
    PortAllocation(std::io::Error),

    #[error("Failed to spawn sandbox process: {0}")]
    Spawn(std::io::Error),

    #[error("Filesystem error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("Sandbox template invalid: {0}")]
    TemplateInvalid(String),
}

/// Errors from [`crate::InstanceClient`] HTTP calls.
#[derive(Debug, Error)]
pub enum InstanceError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("Server returned error: {0}")]
    Server(String),

    #[error("Decode error: {0}")]
    Decode(String),
}
