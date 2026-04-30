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
    #[error("LibraryError - SpaceCreate: {0}")]
    SpaceCreate(#[from] super::space::repo::SpaceCreateError),
    #[error("LibraryError - SpaceFind: {0}")]
    SpaceFind(#[from] super::space::repo::SpaceFindError),
    #[error("LibraryError - SpaceQuery: {0}")]
    SpaceQuery(#[from] super::space::repo::SpaceQueryError),
    #[error("LibraryError - Job: {0}")]
    Job(#[from] ::job::error::JobError),
    #[error(
        "LibraryError - SpaceInitFailed: scaffolding commit for space {slug:?} did not complete"
    )]
    SpaceInitFailed { slug: String },
}
