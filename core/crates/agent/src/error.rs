use thiserror::Error;

use super::repo::{AgentCreateError, AgentFindError, AgentModifyError};
use super::session::error::AgentSessionError;

#[derive(Error, Debug)]
pub enum AgentError {
    #[error("AgentError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("AgentError - Create: {0}")]
    Create(#[from] AgentCreateError),
    #[error("AgentError - Modify: {0}")]
    Modify(#[from] AgentModifyError),
    #[error("AgentError - Find: {0}")]
    Find(#[from] AgentFindError),
    #[error("AgentError - Session: {0}")]
    Session(#[from] AgentSessionError),
}
