use thiserror::Error;

use crate::agent::AgentError;

use super::repo::{
    WorkspaceCreateError, WorkspaceFindError, WorkspaceModifyError, WorkspaceQueryError,
};

#[derive(Error, Debug)]
pub enum WorkspaceError {
    #[error("WorkspaceError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("WorkspaceError - Create: {0}")]
    Create(#[from] WorkspaceCreateError),
    #[error("WorkspaceError - Modify: {0}")]
    Modify(#[from] WorkspaceModifyError),
    #[error("WorkspaceError - Find: {0}")]
    Find(#[from] WorkspaceFindError),
    #[error("WorkspaceError - Agent: {0}")]
    Agent(#[from] AgentError),
    #[error("WorkspaceError - Query: {0}")]
    Query(#[from] WorkspaceQueryError),
}
