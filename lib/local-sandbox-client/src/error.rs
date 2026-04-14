use thiserror::Error;

#[derive(Debug, Error)]
pub enum LocalSandboxError {
    #[error("Sandbox not found: {0}")]
    NotFound(String),

    #[error("Sandbox already exists: {0}")]
    AlreadyExists(String),

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

    #[error("Timed out waiting for sandbox {0} to become ready")]
    Timeout(String),
}
