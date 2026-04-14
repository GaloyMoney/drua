use thiserror::Error;

#[derive(Error, Debug)]
pub enum SlashCommandError {
    #[error("SlashCommandError - NotFound: {0}")]
    NotFound(String),
    #[error("SlashCommandError - ExecutionFailed: {0}")]
    ExecutionFailed(String),
}
