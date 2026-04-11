use thiserror::Error;

#[derive(Error, Debug)]
pub enum SessionError {
    #[error("SessionError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("SessionError - SessionNotFound")]
    SessionNotFound,
}
