/// Wraps drua_library's error types and folds in auth + space-init
/// failures that surface through the auth-gated wrappers.
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
}
