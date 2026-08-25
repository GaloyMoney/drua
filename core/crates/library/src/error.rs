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
    #[error("job: {0}")]
    Job(#[from] job::error::JobError),
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("head publish: {0}")]
    HeadPublish(String),
    #[error("local library at version {applied}, needs {required}: catch-up timed out")]
    CatchUpTimeout { required: u64, applied: u64 },
}
