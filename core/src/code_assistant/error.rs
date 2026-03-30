use thiserror::Error;

#[derive(Error, Debug)]
pub enum CodeAssistantError {
    #[error("CodeAssistantError - Search: {0}")]
    Search(String),
    #[error("CodeAssistantError - InvalidLabel: {0}")]
    InvalidLabel(String),
    #[error("CodeAssistantError - Init: {0}")]
    Init(String),
}
