use thiserror::Error;

use super::repo::{MemoryCreateError, MemoryFindError, MemoryModifyError};

#[derive(Error, Debug)]
pub enum MemoryError {
    #[error("MemoryError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("MemoryError - Create: {0}")]
    Create(#[from] MemoryCreateError),
    #[error("MemoryError - Modify: {0}")]
    Modify(#[from] MemoryModifyError),
    #[error("MemoryError - Find: {0}")]
    Find(#[from] MemoryFindError),
    #[error("MemoryError - Json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("MemoryError - Embedding: {0}")]
    Embedding(String),
    #[error("MemoryError - NotFound: {0}")]
    NotFound(String),
}
