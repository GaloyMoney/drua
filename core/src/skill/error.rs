use thiserror::Error;

use crate::auth::error::AuthorizationError;

use super::repo::{
    SkillColumn, SkillCreateError, SkillFindError, SkillModifyError, SkillQueryError,
};

#[derive(Error, Debug)]
pub enum SkillError {
    #[error("SkillError - Create: {0}")]
    Create(SkillCreateError),
    #[error("SkillError - Modify: {0}")]
    Modify(SkillModifyError),
    #[error("SkillError - Find: {0}")]
    Find(#[from] SkillFindError),
    #[error("SkillError - Query: {0}")]
    Query(#[from] SkillQueryError),
    #[error("SkillError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("SkillError - SandboxLookup: {0}")]
    SandboxLookup(String),
    #[error("SkillError - BuildEntity: {0}")]
    BuildEntity(String),
    #[error("SkillError - Library: {0}")]
    Library(#[from] crate::library::LibraryError),
    #[error("SkillError - Drua: {0}")]
    Drua(#[from] drua_library::LibraryError),
    #[error("SkillError - Authorization: {0}")]
    Authorization(#[from] AuthorizationError),
    #[error("SkillError - skill is space-scoped; delete via the spaces tool")]
    SpaceScopedDeleteViaSpaces,
    #[error(
        "SkillError - skill `{name}` is owned by space `{space_slug}`; \
         edit it via the spaces tool (`spaces edit op=write slug={space_slug} \
         path={path}`) — the project surface only manages project-scoped skills"
    )]
    SpaceScopedEditViaSpaces {
        name: String,
        space_slug: String,
        path: String,
    },
    /// A name + scope collision tripped one of the partial unique indexes
    /// (`idx_skills_global_name`, `idx_skills_project_name`, or
    /// `idx_skills_space_name`). Surfaced as a typed variant so callers
    /// can render a clear message instead of leaking a generic SQL error.
    #[error("SkillError - duplicate name in scope: {0}")]
    DuplicateName(String),
}

impl From<SkillCreateError> for SkillError {
    fn from(e: SkillCreateError) -> Self {
        if let SkillCreateError::ConstraintViolation { column, value, .. } = &e {
            if matches!(
                column,
                Some(SkillColumn::Name | SkillColumn::ProjectId | SkillColumn::SpaceId)
            ) {
                return SkillError::DuplicateName(value.clone().unwrap_or_default());
            }
        }
        SkillError::Create(e)
    }
}

impl From<SkillModifyError> for SkillError {
    fn from(e: SkillModifyError) -> Self {
        if let SkillModifyError::ConstraintViolation { column, value, .. } = &e {
            if matches!(
                column,
                Some(SkillColumn::Name | SkillColumn::ProjectId | SkillColumn::SpaceId)
            ) {
                return SkillError::DuplicateName(value.clone().unwrap_or_default());
            }
        }
        SkillError::Modify(e)
    }
}
