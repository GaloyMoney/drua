use super::repo::{NoteCreateError, NoteFindError, NoteModifyError, NoteQueryError};

/// Maximum allowed length for note content in characters.
pub const MAX_NOTE_CONTENT_LEN: usize = 4000;

#[derive(thiserror::Error, Debug)]
pub enum NoteError {
    #[error("NoteError - ContentTooLarge: content is {0} chars, max {MAX_NOTE_CONTENT_LEN}")]
    ContentTooLarge(usize),
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
    #[error("NoteError - Drua: {0}")]
    Drua(#[from] drua_library::LibraryError),
    #[error("NoteError - BuildEntity: {0}")]
    BuildEntity(String),
    #[error("Space-scoped notes must be deleted via the spaces tool (`spaces edit op=delete`)")]
    SpaceScopedDeleteViaSpaces,
}
