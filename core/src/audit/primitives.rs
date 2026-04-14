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
    /// Sandbox targeted by this interaction. Set by sandbox-service
    /// methods via [`crate::audit::Audit::record_sandbox_id`] so audit
    /// log queries can filter by sandbox.
    pub sandbox_id: Option<SandboxId>,
    pub action: Option<String>,
    pub outcome: Option<InteractionOutcome>,
    pub duration_ms: Option<u64>,
    pub tokens_returned: Option<u64>,
    pub metadata: Option<serde_json::Value>,
}

/// Filter criteria for querying audit log entries.
///
/// All fields are optional — unset fields are not included in the WHERE
/// clause. String fields use `ILIKE` for fuzzy matching.
#[derive(Debug, Clone, Default)]
pub struct AuditLogQuery {
    /// Only entries for this workspace.
    pub workspace_id: Option<WorkspaceId>,
    /// Only entries by this acting user.
    pub acting_user_id: Option<UserId>,
    /// Only entries by this acting agent.
    pub acting_agent_id: Option<AgentId>,
    /// Exclude entries by this agent (e.g. to hide the caller's own logs).
    pub exclude_agent_id: Option<AgentId>,
    /// Only entries for this sandbox.
    pub sandbox_id: Option<SandboxId>,
    /// Substring match on the `action` column.
    pub action: Option<String>,
    /// Substring match on the `outcome` column (e.g. "success", "error").
    pub outcome: Option<String>,
    /// Only entries where `error` is this value.
    pub error: Option<bool>,
    /// Maximum number of rows (clamped to 1..=100, default 20).
    pub limit: i64,
}

/// A recorded audit entry.
#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub id: AuditEntryId,
    pub acting_user_id: Option<UserId>,
    pub workspace_id: Option<WorkspaceId>,
    pub acting_agent_id: Option<AgentId>,
    pub on_behalf_of_user_id: Option<UserId>,
    pub sandbox_id: Option<SandboxId>,
    pub interaction_type: String,
    pub action: String,
    pub metadata: serde_json::Value,
    pub outcome: String,
    pub error: Option<bool>,
    pub duration_ms: Option<i64>,
    pub tokens_returned: Option<i64>,
    pub recorded_at: chrono::DateTime<chrono::Utc>,
}
