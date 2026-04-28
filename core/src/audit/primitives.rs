use serde::{Deserialize, Serialize};

use crate::primitives::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type)]
#[sqlx(transparent)]
pub struct AuditEntryId(i64);

impl std::fmt::Display for AuditEntryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<AuditEntryId> for i64 {
    fn from(value: AuditEntryId) -> i64 {
        value.0
    }
}

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

/// Audit fields accumulated via [`EventContext`] during a request.
///
/// Resource IDs are stored in [`resource_ids`](Self::resource_ids) as a flat
/// JSON object mapping to the `resource_ids JSONB` column, so new resource
/// types can be added without schema migrations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuditContextData {
    pub acting_user_id: Option<UserId>,
    pub acting_agent_id: Option<AgentId>,
    pub on_behalf_of_user_id: Option<UserId>,
    #[serde(default)]
    pub resource_ids: serde_json::Map<String, serde_json::Value>,
    pub entrypoint: Option<String>,
    pub interaction_type: Option<InteractionType>,
    pub action: Option<String>,
    pub outcome: Option<InteractionOutcome>,
    pub duration_ms: Option<u64>,
    pub tokens_returned: Option<u64>,
    pub metadata: Option<serde_json::Value>,
}

/// Unset fields are excluded from the WHERE clause; strings use `ILIKE`.
#[derive(Debug, Clone, Default)]
pub struct AuditLogQuery {
    pub workspace_id: Option<WorkspaceId>,
    pub acting_user_id: Option<UserId>,
    pub acting_agent_id: Option<AgentId>,
    /// Exclude entries by this agent (e.g. to hide the caller's own logs).
    pub exclude_agent_id: Option<AgentId>,
    pub sandbox_id: Option<SandboxId>,
    pub entrypoint: Option<String>,
    pub action: Option<String>,
    pub outcome: Option<String>,
    pub error: Option<bool>,
    /// Clamped to 1..=100, default 20.
    pub limit: i64,
}

#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub id: AuditEntryId,
    pub acting_user_id: Option<UserId>,
    pub acting_agent_id: Option<AgentId>,
    pub on_behalf_of_user_id: Option<UserId>,
    pub resource_ids: serde_json::Value,
    /// `None` for entries recorded before this column was introduced.
    pub entrypoint: Option<String>,
    pub interaction_type: String,
    pub action: String,
    pub metadata: serde_json::Value,
    pub outcome: String,
    pub error: Option<bool>,
    /// Error message text recorded for failed interactions. Truncated to
    /// [`MAX_ERROR_MESSAGE_BYTES`](super::MAX_ERROR_MESSAGE_BYTES) at
    /// insertion time. `None` for successful interactions or rows
    /// recorded before this column was introduced.
    pub error_message: Option<String>,
    pub duration_ms: Option<i64>,
    pub tokens_returned: Option<i64>,
    pub recorded_at: chrono::DateTime<chrono::Utc>,
}

impl AuditEntry {
    pub fn resource_id(&self, key: &str) -> Option<&str> {
        self.resource_ids.get(key).and_then(|v| v.as_str())
    }

    pub fn workspace_id(&self) -> Option<WorkspaceId> {
        self.resource_id("workspace_id")
            .and_then(|s| s.parse::<uuid::Uuid>().ok())
            .map(WorkspaceId::from)
    }

    pub fn sandbox_id(&self) -> Option<SandboxId> {
        self.resource_id("sandbox_id")
            .and_then(|s| s.parse::<uuid::Uuid>().ok())
            .map(SandboxId::from)
    }
}
