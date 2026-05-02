/// Crate-local Library error. Wraps drua_library's error type and
/// folds in auth + space repo errors that surface through the library
/// API.
#[derive(thiserror::Error, Debug)]
pub enum LibraryError {
    #[error("LibraryError - drua_library: {0}")]
    Drua(#[from] drua_library::LibraryError),
    #[error("LibraryError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("LibraryError - Authorization: {0}")]
    Authorization(#[from] crate::auth::error::AuthorizationError),
    #[error("LibraryError - Space: {0}")]
    Space(#[from] drua_library::SpaceError),
    #[error("LibraryError - Job: {0}")]
    Job(#[from] ::job::error::JobError),
    #[error(
        "LibraryError - SpaceInitFailed: scaffolding commit for space {slug:?} did not complete"
    )]
    SpaceInitFailed { slug: String },
    #[error("LibraryError - SpaceOpFailed: upstream operation on space {slug:?} did not complete")]
    SpaceOpFailed { slug: String },
}
