#[derive(Debug, thiserror::Error)]
pub enum LibraryError {
    #[error("config: {0}")]
    Config(String),
    #[error("git: {0}")]
    Git(String),
    #[error("io: {0}")]
    Io(String),
    #[error("validation: {0}")]
    Validation(String),
    #[error(
        "ref {refname} changed underneath the caller: expected {expected}, origin now has {actual}"
    )]
    RefChanged {
        refname: String,
        expected: String,
        actual: String,
    },
    #[error("job: {0}")]
    Job(#[from] job::error::JobError),
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
}
