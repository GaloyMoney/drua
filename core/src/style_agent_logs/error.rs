use thiserror::Error;

#[derive(Error, Debug)]
pub enum StyleAgentLogsError {
    #[error("StyleAgentLogsError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
}
