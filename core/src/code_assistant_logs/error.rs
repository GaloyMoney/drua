use thiserror::Error;

#[derive(Error, Debug)]
pub enum CodeAssistantLogsError {
    #[error("CodeAssistantLogsError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
}
