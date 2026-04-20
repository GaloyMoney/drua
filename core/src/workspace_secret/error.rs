use thiserror::Error;

use crate::auth::error::AuthorizationError;
use crate::encryption::EncryptionError;

use super::repo::{
    WorkspaceSecretCreateError, WorkspaceSecretFindError, WorkspaceSecretModifyError,
    WorkspaceSecretQueryError,
};

#[derive(Error, Debug)]
pub enum WorkspaceSecretError {
    #[error("WorkspaceSecretError - Create: {0}")]
    Create(#[from] WorkspaceSecretCreateError),
    #[error("WorkspaceSecretError - Modify: {0}")]
    Modify(#[from] WorkspaceSecretModifyError),
    #[error("WorkspaceSecretError - Find: {0}")]
    Find(#[from] WorkspaceSecretFindError),
    #[error("WorkspaceSecretError - Query: {0}")]
    Query(#[from] WorkspaceSecretQueryError),
    #[error("WorkspaceSecretError - Encryption: {0}")]
    Encryption(#[from] EncryptionError),
    #[error("WorkspaceSecretError - Authorization: {0}")]
    Authorization(#[from] AuthorizationError),
}
