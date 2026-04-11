use thiserror::Error;

#[derive(Error, Debug)]
pub enum WorkspaceSecretError {
    #[error("WorkspaceSecretError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("WorkspaceSecretError - SecretNotFound")]
    SecretNotFound,
    #[error("WorkspaceSecretError - InvalidSecretType: {0}")]
    InvalidSecretType(String),
}
