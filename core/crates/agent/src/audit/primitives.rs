use serde::{Deserialize, Serialize};

use primitives::*;

use crate::auth::AuthSubject;

/// Auto-incrementing audit entry identifier (BIGSERIAL).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type)]
#[sqlx(transparent)]
pub struct AuditEntryId(i64);

impl std::fmt::Display for AuditEntryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The type of service boundary interaction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InteractionType {
    ApiCall,
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
    User {
        user_id: UserId,
    },
    ExportedAgent {
        mcp_creds_id: McpCredsId,
        user_id: UserId,
    },
    Agent {
        workspace_id: WorkspaceId,
        agent_id: AgentId,
    },
    Anonymous,
}

impl std::fmt::Display for AuditSubject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuditSubject::User { user_id } => write!(f, "user::{user_id}"),
            AuditSubject::ExportedAgent {
                mcp_creds_id,
                user_id: _,
            } => write!(f, "exported_agent::{mcp_creds_id}"),
            AuditSubject::Agent {
                workspace_id,
                agent_id,
            } => write!(f, "agent::{agent_id}::ws:{workspace_id}"),
            AuditSubject::Anonymous => write!(f, "anonymous"),
        }
    }
}

impl From<&AuthSubject> for AuditSubject {
    fn from(ctx: &AuthSubject) -> Self {
        match ctx {
            AuthSubject::User(user_id) => AuditSubject::User { user_id: *user_id },
            AuthSubject::ExportedAgent(user_id, mcp_creds_id, _) => AuditSubject::ExportedAgent {
                mcp_creds_id: *mcp_creds_id,
                user_id: *user_id,
            },
            AuthSubject::Agent(workspace_id, agent_id, _) => AuditSubject::Agent {
                workspace_id: *workspace_id,
                agent_id: *agent_id,
            },
            AuthSubject::Anonymous => AuditSubject::Anonymous,
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

impl std::fmt::Display for InteractionOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InteractionOutcome::Success => write!(f, "success"),
            InteractionOutcome::Error { .. } => write!(f, "error"),
            InteractionOutcome::Unauthorized => write!(f, "unauthorized"),
        }
    }
}

/// A recorded audit entry.
#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub id: AuditEntryId,
    pub subject: String,
    pub interaction_type: String,
    pub action: String,
    pub metadata: serde_json::Value,
    pub outcome: String,
    pub duration_ms: Option<i64>,
    pub tokens_returned: Option<i64>,
    pub recorded_at: chrono::DateTime<chrono::Utc>,
}
