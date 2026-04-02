use thiserror::Error;

use super::repo::{ReportCreateError, ReportFindError, ReportModifyError};

#[derive(Error, Debug)]
pub enum ReportError {
    #[error("ReportError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("ReportError - Create: {0}")]
    Create(#[from] ReportCreateError),
    #[error("ReportError - Modify: {0}")]
    Modify(#[from] ReportModifyError),
    #[error("ReportError - Find: {0}")]
    Find(#[from] ReportFindError),
    #[error("ReportError - Json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("ReportError - Embedding: {0}")]
    Embedding(String),
    #[error("ReportError - NotFound: {0}")]
    NotFound(String),
}
