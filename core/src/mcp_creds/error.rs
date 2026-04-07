use thiserror::Error;

use super::repo::{
    McpCredsCreateError, McpCredsFindError, McpCredsModifyError, McpCredsQueryError,
};

#[derive(Error, Debug)]
pub enum McpCredsError {
    #[error("McpCredsError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("McpCredsError - Create: {0}")]
    Create(#[from] McpCredsCreateError),
    #[error("McpCredsError - Modify: {0}")]
    Modify(#[from] McpCredsModifyError),
    #[error("McpCredsError - Find: {0}")]
    Find(#[from] McpCredsFindError),
    #[error("McpCredsError - Query: {0}")]
    Query(#[from] McpCredsQueryError),
    #[error("McpCredsError - AlreadyRevoked")]
    AlreadyRevoked,
    #[error("McpCredsError - AuthorizationError")]
    AuthorizationError,
}
