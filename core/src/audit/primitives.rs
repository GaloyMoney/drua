use serde::{Deserialize, Serialize};

use crate::auth::AuthSubject;
use crate::primitives::*;

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
    AgentOnBehalfOfUser {
        user_id: UserId,
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
                user_id,
            } => write!(f, "exported_agent::{mcp_creds_id}::user:{user_id}"),
            AuditSubject::Agent {
                workspace_id,
                agent_id,
            } => write!(f, "agent::{agent_id}::ws:{workspace_id}"),
            AuditSubject::AgentOnBehalfOfUser {
                user_id,
                workspace_id,
                agent_id,
            } => write!(
                f,
                "agent_on_behalf_of_user::{agent_id}::ws:{workspace_id}::user:{user_id}"
            ),
            AuditSubject::Anonymous => write!(f, "anonymous"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ParseAuditSubjectError {
    #[error("AuditSubject - unknown format: {0}")]
    UnknownFormat(String),
    #[error("AuditSubject - malformed `{kind}` subject: {reason}")]
    Malformed {
        kind: &'static str,
        reason: String,
    },
}

impl std::str::FromStr for AuditSubject {
    type Err = ParseAuditSubjectError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "anonymous" {
            return Ok(AuditSubject::Anonymous);
        }

        if let Some(rest) = s.strip_prefix("user::") {
            let user_id = parse_id(rest, "user")?;
            return Ok(AuditSubject::User { user_id });
        }

        if let Some(rest) = s.strip_prefix("exported_agent::") {
            // Tolerate the legacy `exported_agent::<creds>` shape that
            // Display used to emit before the user_id was appended.
            let (creds_str, user_id) = match rest.split_once("::user:") {
                Some((c, u)) => (c, parse_id::<UserId>(u, "exported_agent")?),
                None => (rest, UserId::from(uuid::Uuid::nil())),
            };
            return Ok(AuditSubject::ExportedAgent {
                mcp_creds_id: parse_id(creds_str, "exported_agent")?,
                user_id,
            });
        }

        // Order matters: `agent_on_behalf_of_user::` must be matched before
        // `agent::` since both share the same prefix.
        if let Some(rest) = s.strip_prefix("agent_on_behalf_of_user::") {
            let (agent_str, after) = rest
                .split_once("::ws:")
                .ok_or_else(|| malformed("agent_on_behalf_of_user", "missing ::ws:"))?;
            let (ws_str, user_str) = after
                .split_once("::user:")
                .ok_or_else(|| malformed("agent_on_behalf_of_user", "missing ::user:"))?;
            return Ok(AuditSubject::AgentOnBehalfOfUser {
                agent_id: parse_id(agent_str, "agent_on_behalf_of_user")?,
                workspace_id: parse_id(ws_str, "agent_on_behalf_of_user")?,
                user_id: parse_id(user_str, "agent_on_behalf_of_user")?,
            });
        }

        if let Some(rest) = s.strip_prefix("agent::") {
            let (agent_str, ws_str) = rest
                .split_once("::ws:")
                .ok_or_else(|| malformed("agent", "missing ::ws:"))?;
            return Ok(AuditSubject::Agent {
                agent_id: parse_id(agent_str, "agent")?,
                workspace_id: parse_id(ws_str, "agent")?,
            });
        }

        Err(ParseAuditSubjectError::UnknownFormat(s.to_string()))
    }
}

fn parse_id<T>(s: &str, kind: &'static str) -> Result<T, ParseAuditSubjectError>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    s.parse::<T>().map_err(|e| malformed(kind, e.to_string()))
}

fn malformed(kind: &'static str, reason: impl Into<String>) -> ParseAuditSubjectError {
    ParseAuditSubjectError::Malformed {
        kind,
        reason: reason.into(),
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
            AuthSubject::AgentOnBehalfOfUser(user_id, workspace_id, agent_id, _) => {
                AuditSubject::AgentOnBehalfOfUser {
                    user_id: *user_id,
                    workspace_id: *workspace_id,
                    agent_id: *agent_id,
                }
            }
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
