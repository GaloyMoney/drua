use thiserror::Error;

use crate::auth::error::AuthorizationError;
use crate::encryption::EncryptionError;

use super::repo::{
    ProjectSecretCreateError, ProjectSecretFindError, ProjectSecretModifyError,
    ProjectSecretQueryError,
};

#[derive(Error, Debug)]
pub enum ProjectSecretError {
    #[error("ProjectSecretError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("ProjectSecretError - Create: {0}")]
    Create(#[from] ProjectSecretCreateError),
    #[error("ProjectSecretError - Modify: {0}")]
    Modify(#[from] ProjectSecretModifyError),
    #[error("ProjectSecretError - Find: {0}")]
    Find(#[from] ProjectSecretFindError),
    #[error("ProjectSecretError - Query: {0}")]
    Query(#[from] ProjectSecretQueryError),
    #[error("ProjectSecretError - Encryption: {0}")]
    Encryption(#[from] EncryptionError),
    #[error("ProjectSecretError - Authorization: {0}")]
    Authorization(#[from] AuthorizationError),
}
