use thiserror::Error;

use super::repo::{UserCreateError, UserFindError, UserModifyError, UserQueryError};

#[derive(Error, Debug)]
pub enum UserError {
    #[error("UserError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("UserError - Create: {0}")]
    Create(#[from] UserCreateError),
    #[error("UserError - Modify: {0}")]
    Modify(#[from] UserModifyError),
    #[error("UserError - Find: {0}")]
    Find(#[from] UserFindError),
    #[error("UserError - Query: {0}")]
    Query(#[from] UserQueryError),
}
