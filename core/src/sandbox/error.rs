use thiserror::Error;

use sandbox::AdminError;

use crate::primitives::{AgentId, WorkspaceId};

use super::repo::{SandboxCreateError, SandboxFindError, SandboxModifyError, SandboxQueryError};

#[derive(Error, Debug)]
pub enum SandboxError {
    #[error("SandboxError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("SandboxError - Create: {0}")]
    Create(#[from] SandboxCreateError),
    #[error("SandboxError - Modify: {0}")]
    Modify(#[from] SandboxModifyError),
    #[error("SandboxError - Find: {0}")]
    Find(#[from] SandboxFindError),
    #[error("SandboxError - Query: {0}")]
    Query(#[from] SandboxQueryError),
    #[error("SandboxError - Admin: {0}")]
    Admin(#[from] AdminError),
    #[error("SandboxError - Hydration: {0}")]
    Hydration(#[from] es_entity::EntityHydrationError),
    #[error("SandboxError - sandbox does not belong to workspace {expected} (actual: {actual})")]
    WrongWorkspace {
        expected: WorkspaceId,
        actual: WorkspaceId,
    },
    #[error("SandboxError - write slot already taken by agent {current_writer}")]
    WriteSlotTaken { current_writer: AgentId },
    #[error(
        "SandboxError - sandbox is in {state} state and has no live instance (must be Ready)"
    )]
    NotReady { state: String },
}
