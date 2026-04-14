use thiserror::Error;

use sandbox::AdminError;

use super::repo::{
    SandboxCreateError, SandboxFindError, SandboxModifyError, SandboxQueryError,
};

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
}
