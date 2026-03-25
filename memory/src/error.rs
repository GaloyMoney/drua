/// Errors for the memory crate.
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("embedding error: {0}")]
    Embedding(#[from] anyhow::Error),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("{0}")]
    Other(String),
}
