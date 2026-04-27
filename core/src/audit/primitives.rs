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

impl From<AuditEntryId> for i64 {
    fn from(value: AuditEntryId) -> i64 {
        value.0
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
///
/// Actor columns (`acting_user_id`, `acting_agent_id`, `on_behalf_of_user_id`)
/// remain top-level for fast filtering. Resource IDs (workspace, sandbox, …)
/// are accumulated in [`resource_ids`](Self::resource_ids) as a flat JSON
/// object, mapping directly to the `resource_ids JSONB` column in Postgres so
/// new resource types can be added without schema migrations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuditContextData {
    pub acting_user_id: Option<UserId>,
    pub acting_agent_id: Option<AgentId>,
    pub on_behalf_of_user_id: Option<UserId>,
    /// Resource IDs involved in this interaction, keyed by role
    /// (e.g. `"workspace_id"`, `"sandbox_id"`).
    #[serde(default)]
    pub resource_ids: serde_json::Map<String, serde_json::Value>,
    /// How the request arrived (e.g. `"api: POST /workspaces"`,
    /// `"mcp: bash"`, `"graphql: CreateWorkspace"`).
    pub entrypoint: Option<String>,
    pub interaction_type: Option<InteractionType>,
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
    /// Substring match on the `entrypoint` column.
    pub entrypoint: Option<String>,
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
    pub acting_agent_id: Option<AgentId>,
    pub on_behalf_of_user_id: Option<UserId>,
    /// All resource IDs involved in this interaction as a flat JSON object
    /// (e.g. `{"workspace_id": "…", "sandbox_id": "…"}`).
    pub resource_ids: serde_json::Value,
    /// How the request arrived (e.g. `"api: POST /workspaces"`,
    /// `"mcp: bash"`, `"graphql: CreateWorkspace"`). `None` for
    /// entries recorded before this column was introduced.
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
    /// Extract a resource ID string by key.
    pub fn resource_id(&self, key: &str) -> Option<&str> {
        self.resource_ids.get(key).and_then(|v| v.as_str())
    }

    /// Workspace involved in this interaction, if any.
    pub fn workspace_id(&self) -> Option<WorkspaceId> {
        self.resource_id("workspace_id")
            .and_then(|s| s.parse::<uuid::Uuid>().ok())
            .map(WorkspaceId::from)
    }

    /// Sandbox involved in this interaction, if any.
    pub fn sandbox_id(&self) -> Option<SandboxId> {
        self.resource_id("sandbox_id")
            .and_then(|s| s.parse::<uuid::Uuid>().ok())
            .map(SandboxId::from)
    }
}
