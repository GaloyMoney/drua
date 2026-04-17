use thiserror::Error;

use super::repo::{AgentSessionCreateError, AgentSessionFindError, AgentSessionModifyError};

#[derive(Error, Debug)]
pub enum AgentSessionError {
    #[error("AgentSessionError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("AgentSessionError - Create: {0}")]
    Create(#[from] AgentSessionCreateError),
    #[error("AgentSessionError - Modify: {0}")]
    Modify(#[from] AgentSessionModifyError),
    #[error("AgentSessionError - Find: {0}")]
    Find(#[from] AgentSessionFindError),
    #[error("AgentSessionError - cannot append a user message after another user message")]
    ConsecutiveUserMessages,
    #[error("AgentSessionError - thread not found")]
    ThreadNotFound,
}
