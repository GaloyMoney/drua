use thiserror::Error;

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
    #[error("ProjectError - Query: {0}")]
    Query(#[from] ProjectQueryError),
    #[error("ProjectError - NameAlreadyExists: {0}")]
    NameAlreadyExists(String),
}
