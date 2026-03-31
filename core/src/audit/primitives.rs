use serde::{Deserialize, Serialize};

use crate::primitives::*;

/// The type of service boundary interaction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InteractionType {
    /// HTTP API call (web routes)
    ApiCall,
    /// MCP gateway tool call
    McpCall,
}

impl std::fmt::Display for InteractionType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InteractionType::ApiCall => write!(f, "api_call"),
            InteractionType::McpCall => write!(f, "mcp_call"),
        }
    }
}

/// Who initiated the interaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuditSubject {
    User { user_id: UserId },
    Agent { agent_id: AgentId, user_id: UserId },
    Anonymous,
}

impl std::fmt::Display for AuditSubject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuditSubject::User { user_id } => write!(f, "user::{user_id}"),
            AuditSubject::Agent {
                agent_id,
                user_id: _,
            } => write!(f, "agent::{agent_id}"),
            AuditSubject::Anonymous => write!(f, "anonymous"),
        }
    }
}

/// Outcome of the interaction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InteractionOutcome {
    Success,
    Error { message: String },
    Unauthorized,
}
