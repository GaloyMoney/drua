use thiserror::Error;

#[derive(Error, Debug)]
pub enum ChatHistoryError {
    #[error("ChatHistoryError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("ChatHistoryError - ConversationNotFound")]
    ConversationNotFound,
}
