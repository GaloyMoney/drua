#[derive(thiserror::Error, Debug)]
pub enum LibraryError {
    #[error("LibraryError - IO: {0}")]
    Io(String),
    #[error("LibraryError - Git: {0}")]
    Git(String),
    #[error("LibraryError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
}
