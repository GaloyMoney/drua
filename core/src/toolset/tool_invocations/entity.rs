use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::auth::AuthSubject;
use crate::primitives::{AgentId, ToolInvocationId, UserId};

/// Owner of a persisted invocation. Mirrors `McpCredsOwner`: a sum over
/// `(UserId, AgentId)` so mcp-gateway external callers (no agent in
/// scope) can still surface fetch handles, with `Anonymous` as the only
/// shape that yields no scope at all.
///
/// When both an agent_id and a user_id are available on the subject
/// (`AgentOnBehalfOfUser`), the agent wins — it's the more granular
/// scope key for cleanup-on-delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolInvocationOwner {
    Agent { agent_id: AgentId },
    User { user_id: UserId },
}

impl ToolInvocationOwner {
    /// Derive the owner from an `AuthSubject`. `None` for `Anonymous` —
    /// the dispatcher then short-circuits to raw passthrough.
    pub fn from_subject(subject: &AuthSubject) -> Option<Self> {
        // `AgentOnBehalfOfUser` resolves to the agent (more granular FK
        // for cleanup). User-only subjects (`User`, `ExportedAgent`)
        // fall back to `originating_user_id`. Pure `Anonymous` and
        // autonomous `Agent`-without-user paths are handled by the two
        // calls in priority order.
        if let Some(agent_id) = subject.acting_agent_id() {
            return Some(Self::Agent { agent_id });
        }
        subject
            .originating_user_id()
            .map(|user_id| Self::User { user_id })
    }

    pub fn agent_id(&self) -> Option<AgentId> {
        match self {
            Self::Agent { agent_id } => Some(*agent_id),
            Self::User { .. } => None,
        }
    }

    pub fn user_id(&self) -> Option<UserId> {
        match self {
            Self::User { user_id } => Some(*user_id),
            Self::Agent { .. } => None,
        }
    }
}

/// One row from `tool_invocations`. Append-once after creation; all reads
/// hit the same row by id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInvocation {
    pub id: ToolInvocationId,
    pub owner: ToolInvocationOwner,
    pub tool_name: String,
    pub args: serde_json::Value,
    pub args_hash: Vec<u8>,
    pub classifier: String,
    pub summary: serde_json::Value,
    pub raw_text: String,
    pub raw_size_bytes: i64,
    pub exit_code: Option<i32>,
    pub duration_ms: i32,
    pub started_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

/// Insert-only payload for [`ToolInvocations::persist`]. Plain struct rather
/// than a derive_builder shape because every field is required at dispatch
/// time and the call sites are internal to the dispatcher.
#[derive(Debug, Clone)]
pub struct NewToolInvocation {
    pub owner: ToolInvocationOwner,
    pub tool_name: String,
    pub args: serde_json::Value,
    pub args_hash: Vec<u8>,
    pub classifier: String,
    pub summary: serde_json::Value,
    pub raw_text: String,
    pub raw_size_bytes: i64,
    pub exit_code: Option<i32>,
    pub duration_ms: i32,
    pub started_at: DateTime<Utc>,
}
