use thiserror::Error;

#[derive(Error, Debug)]
pub enum AuditError {
    #[error("AuditError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
}
