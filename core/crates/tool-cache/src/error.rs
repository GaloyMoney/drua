use thiserror::Error;

#[derive(Error, Debug)]
pub enum ToolInvocationError {
    #[error("ToolInvocationError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("ToolInvocationError - InvalidPattern: {0}")]
    InvalidPattern(String),
    #[error("ToolInvocationError - Serde: {0}")]
    Serde(#[from] serde_json::Error),
}
