use super::repo::{NoteCreateError, NoteFindError, NoteModifyError, NoteQueryError};

#[derive(thiserror::Error, Debug)]
pub enum NoteError {
    #[error("NoteError - Create: {0}")]
    Create(#[from] NoteCreateError),
    #[error("NoteError - Modify: {0}")]
    Modify(#[from] NoteModifyError),
    #[error("NoteError - Find: {0}")]
    Find(#[from] NoteFindError),
    #[error("NoteError - Query: {0}")]
    Query(#[from] NoteQueryError),
    #[error("NoteError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("NoteError - Authorization: {0}")]
    Authorization(#[from] crate::primitives::AuthorizationError),
    #[error("NoteError - Library: {0}")]
    Library(#[from] crate::library::LibraryError),
}
