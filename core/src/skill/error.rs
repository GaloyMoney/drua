use thiserror::Error;

use super::repo::{SkillCreateError, SkillFindError, SkillModifyError, SkillQueryError};

#[derive(Error, Debug)]
pub enum SkillError {
    #[error("SkillError - Create: {0}")]
    Create(#[from] SkillCreateError),
    #[error("SkillError - Modify: {0}")]
    Modify(#[from] SkillModifyError),
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
}
