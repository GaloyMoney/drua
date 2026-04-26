use thiserror::Error;

use crate::agent::AgentError;
use crate::auth::error::AuthorizationError;
use crate::library::LibraryError;
use crate::note::NoteError;
use crate::sandbox::SandboxError;
use crate::skill::SkillError;
use crate::workspace_secret::WorkspaceSecretError;

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
    #[error("WorkspaceError - Authorization: {0}")]
    Authorization(#[from] AuthorizationError),
    #[error("WorkspaceError - Library: {0}")]
    Library(#[from] LibraryError),
    #[error("WorkspaceError - Sandbox: {0}")]
    Sandbox(#[from] SandboxError),
    #[error("WorkspaceError - Skill: {0}")]
    Skill(#[from] SkillError),
    #[error("WorkspaceError - Note: {0}")]
    Note(#[from] NoteError),
    #[error("WorkspaceError - WorkspaceSecret: {0}")]
    WorkspaceSecret(#[from] WorkspaceSecretError),
}
