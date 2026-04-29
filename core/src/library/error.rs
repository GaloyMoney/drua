#[derive(thiserror::Error, Debug)]
pub enum LibraryError {
    #[error("LibraryError - IO: {0}")]
    Io(String),
    #[error("LibraryError - Git: {0}")]
    Git(String),
    #[error("LibraryError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("LibraryError - Inbox: {0}")]
    Inbox(#[from] obix::InboxError),
    #[error("LibraryError - Authorization: {0}")]
    Authorization(#[from] crate::auth::error::AuthorizationError),
    /// Cross-service error stringified at the boundary (e.g. a space
    /// lookup failure from inside a library sync job).
    #[error("LibraryError - Other: {0}")]
    Other(String),
}
