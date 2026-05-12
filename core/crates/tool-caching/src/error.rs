#[derive(Debug, thiserror::Error)]
pub enum ToolCachingError {
    #[error("tool-caching sqlx error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("tool-caching serde_json error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("tool invocation not found")]
    InvocationNotFound,
    #[error("invalid fetch path: {0}")]
    InvalidPath(String),
}
