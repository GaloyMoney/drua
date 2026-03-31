use thiserror::Error;

use super::repo::*;

#[derive(Error, Debug)]
pub enum AuditError {
    #[error("AuditError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("AuditError - Create: {0}")]
    Create(#[from] AuditEntryCreateError),
    #[error("AuditError - Find: {0}")]
    Find(#[from] AuditEntryFindError),
    #[error("AuditError - Query: {0}")]
    Query(#[from] AuditEntryQueryError),
}
