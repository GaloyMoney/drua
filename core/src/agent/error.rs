use thiserror::Error;

use super::repo::{AgentCreateError, AgentFindError, AgentModifyError, AgentQueryError};

#[derive(Error, Debug)]
pub enum AgentError {
    #[error("AgentError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("AgentError - Create: {0}")]
    Create(#[from] AgentCreateError),
    #[error("AgentError - Modify: {0}")]
    Modify(#[from] AgentModifyError),
    #[error("AgentError - Find: {0}")]
    Find(#[from] AgentFindError),
    #[error("AgentError - Query: {0}")]
    Query(#[from] AgentQueryError),
    #[error("AgentError - McpCreds: {0}")]
    McpCreds(#[from] crate::mcp_creds::McpCredsError),
    #[error("AgentError - Sandbox: {0}")]
    Sandbox(#[from] sandbox_client::SandboxError),
    #[error("Sandbox runtime is not configured. Enable sandbox in the server configuration to chat with agents.")]
    SandboxNotConfigured,
    #[error("AgentError - SandboxExec: {0}")]
    SandboxExec(String),
    #[error("AgentError - AnthropicApi: status={status}, message={message}")]
    AnthropicApi { status: u16, message: String },
    #[error("AgentError - Http: {0}")]
    Http(reqwest::Error),
    #[error("Light agent is not configured. Set ANTHROPIC_API_KEY and provide a catalog.")]
    LightAgentNotConfigured,
    #[error("AgentError - MaxTurnsReached: {0}")]
    MaxTurnsReached(usize),
    #[error("AgentError - ChannelClosed")]
    ChannelClosed,
    #[error("AgentError - ChatHistory: {0}")]
    ChatHistory(crate::chat_history::ChatHistoryError),
}
