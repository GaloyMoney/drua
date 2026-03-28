use thiserror::Error;

use super::repo::{TaskCreateError, TaskFindError, TaskModifyError, TaskQueryError};

#[derive(Error, Debug)]
pub enum TaskError {
    #[error("TaskError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("TaskError - Create: {0}")]
    Create(#[from] TaskCreateError),
    #[error("TaskError - Modify: {0}")]
    Modify(#[from] TaskModifyError),
    #[error("TaskError - Find: {0}")]
    Find(#[from] TaskFindError),
    #[error("TaskError - Query: {0}")]
    Query(#[from] TaskQueryError),
    #[error("TaskError - InvalidStateTransition: {0}")]
    InvalidStateTransition(String),
}
