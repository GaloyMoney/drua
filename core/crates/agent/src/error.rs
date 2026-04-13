use thiserror::Error;

use super::repo::{AgentCreateError, AgentFindError, AgentModifyError, AgentQueryError};
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
    #[error("AgentError - Query: {0}")]
    Query(#[from] AgentQueryError),
    #[error("AgentError - Session: {0}")]
    Session(#[from] AgentSessionError),
    #[error("AgentError - prompt request channel closed")]
    PromptRequestChannelClosed,
    #[error("AgentError - role not configured: {0:?}")]
    RoleNotConfigured(super::entity::AgentRole),
    #[error("AgentError - unauthorized")]
    Unauthorized,
}
