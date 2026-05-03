use thiserror::Error;

use drua_library::SpaceError;

use crate::agent::AgentError;
use crate::auth::error::AuthorizationError;
use crate::library::LibraryError;
use crate::note::NoteError;
use crate::project_secret::ProjectSecretError;
use crate::sandbox::SandboxError;
use crate::skill::SkillError;

use super::repo::{ProjectCreateError, ProjectFindError, ProjectModifyError, ProjectQueryError};

#[derive(Error, Debug)]
pub enum ProjectError {
    #[error("ProjectError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("ProjectError - Create: {0}")]
    Create(#[from] ProjectCreateError),
    #[error("ProjectError - Modify: {0}")]
    Modify(#[from] ProjectModifyError),
    #[error("ProjectError - Find: {0}")]
    Find(#[from] ProjectFindError),
    #[error("ProjectError - Agent: {0}")]
    Agent(#[from] AgentError),
    #[error("ProjectError - Query: {0}")]
    Query(#[from] ProjectQueryError),
    #[error("ProjectError - Authorization: {0}")]
    Authorization(#[from] AuthorizationError),
    #[error("ProjectError - Library: {0}")]
    Library(#[from] LibraryError),
    #[error("ProjectError - Drua: {0}")]
    Drua(#[from] drua_library::LibraryError),
    #[error("ProjectError - Space: {0}")]
    Space(#[from] SpaceError),
    #[error("ProjectError - Sandbox: {0}")]
    Sandbox(#[from] SandboxError),
    #[error("ProjectError - Skill: {0}")]
    Skill(#[from] SkillError),
    #[error("ProjectError - Note: {0}")]
    Note(#[from] NoteError),
    #[error("ProjectError - ProjectSecret: {0}")]
    ProjectSecret(#[from] ProjectSecretError),
}
