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
    #[error("LibraryError - Space: {0}")]
    Space(#[from] super::space::SpaceError),
}
