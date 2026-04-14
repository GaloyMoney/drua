use serde::{Deserialize, Serialize};

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

/// Outcome of the interaction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InteractionOutcome {
    Success,
    Error { message: String },
}

impl std::fmt::Display for InteractionOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InteractionOutcome::Success => write!(f, "success"),
            InteractionOutcome::Error { .. } => write!(f, "error"),
        }
    }
}

/// Accumulated audit fields collected via [`EventContext`] during a request.
///
/// Each field is optional — callers record fields progressively via the
/// type-safe `Audit::record_*` helpers and a final `Audit::collect_context`
/// reads the snapshot before persistence.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuditContextData {
    pub acting_user_id: Option<UserId>,
    pub workspace_id: Option<WorkspaceId>,
    pub acting_agent_id: Option<AgentId>,
    pub on_behalf_of_user_id: Option<UserId>,
    pub interaction_type: Option<InteractionType>,
    pub action: Option<String>,
    pub outcome: Option<InteractionOutcome>,
    pub duration_ms: Option<u64>,
    pub tokens_returned: Option<u64>,
    pub metadata: Option<serde_json::Value>,
}

/// A recorded audit entry.
#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub id: AuditEntryId,
    pub acting_user_id: Option<UserId>,
    pub workspace_id: Option<WorkspaceId>,
    pub acting_agent_id: Option<AgentId>,
    pub on_behalf_of_user_id: Option<UserId>,
    pub interaction_type: String,
    pub action: String,
    pub metadata: serde_json::Value,
    pub outcome: String,
    pub error: Option<bool>,
    pub duration_ms: Option<i64>,
    pub tokens_returned: Option<i64>,
    pub recorded_at: chrono::DateTime<chrono::Utc>,
}
