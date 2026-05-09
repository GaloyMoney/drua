use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Strongly-typed UUID newtype for the persistence layer. Crate-local
/// so the type doesn't leak `crate::primitives::ToolInvocationId`'s
/// es-entity machinery into callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolInvocationId(pub uuid::Uuid);

impl ToolInvocationId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl Default for ToolInvocationId {
    fn default() -> Self {
        Self::new()
    }
}

impl From<uuid::Uuid> for ToolInvocationId {
    fn from(id: uuid::Uuid) -> Self {
        Self(id)
    }
}

impl From<ToolInvocationId> for uuid::Uuid {
    fn from(id: ToolInvocationId) -> Self {
        id.0
    }
}

impl std::fmt::Display for ToolInvocationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Newtype carrying any UUID that can own a persisted invocation —
/// either an agent id or a user id, the discrimination opaque to
/// this crate. Callers in drua-core (or any other host) provide
/// `From<TheirAgentId> for ToolInvocationOwnerId` and similar so
/// the boundary stays type-safe without this crate having to know
/// about the host's identity types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolInvocationOwnerId(pub uuid::Uuid);

impl From<uuid::Uuid> for ToolInvocationOwnerId {
    fn from(id: uuid::Uuid) -> Self {
        Self(id)
    }
}

impl From<ToolInvocationOwnerId> for uuid::Uuid {
    fn from(id: ToolInvocationOwnerId) -> Self {
        id.0
    }
}

impl std::fmt::Display for ToolInvocationOwnerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Owner of a persisted invocation — flat shape carrying the FK
/// columns the table tracks (`agent_id`, `user_id`), exactly one of
/// which is non-null per row. The crate is auth-system agnostic;
/// callers populate the fields from whatever subject / principal
/// model they use, going through `From` conversions into
/// [`ToolInvocationOwnerId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvocationOwner {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<ToolInvocationOwnerId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<ToolInvocationOwnerId>,
}

impl InvocationOwner {
    pub fn agent(id: impl Into<ToolInvocationOwnerId>) -> Self {
        Self {
            agent_id: Some(id.into()),
            user_id: None,
        }
    }

    pub fn user(id: impl Into<ToolInvocationOwnerId>) -> Self {
        Self {
            agent_id: None,
            user_id: Some(id.into()),
        }
    }

    /// `true` when these rows belong to the same fetch scope. Match
    /// on EITHER dimension: agent-side persistence with a user
    /// attribution (e.g. `AgentOnBehalfOfUser` populates both
    /// `agent_id` and `user_id`) lets the same logical user fetch
    /// it later via an `ExportedAgent` bearer token (which only
    /// supplies `user_id`). Cursor #3212630890.
    pub fn matches(&self, other: &Self) -> bool {
        if let (Some(a), Some(b)) = (self.agent_id, other.agent_id) {
            if a == b {
                return true;
            }
        }
        if let (Some(u), Some(v)) = (self.user_id, other.user_id) {
            if u == v {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u128) -> ToolInvocationOwnerId {
        ToolInvocationOwnerId(uuid::Uuid::from_u128(n))
    }

    #[test]
    fn matches_same_agent() {
        let a = InvocationOwner::agent(id(1));
        let b = InvocationOwner::agent(id(1));
        assert!(a.matches(&b));
    }

    #[test]
    fn matches_same_user() {
        let a = InvocationOwner::user(id(2));
        let b = InvocationOwner::user(id(2));
        assert!(a.matches(&b));
    }

    #[test]
    fn matches_cross_dimension_agent_with_user_attribution() {
        // Stored: AgentOnBehalfOfUser populates both fields.
        let stored = InvocationOwner {
            agent_id: Some(id(1)),
            user_id: Some(id(2)),
        };
        // Fetcher: ExportedAgent supplies user_id only.
        let fetcher = InvocationOwner::user(id(2));
        assert!(
            stored.matches(&fetcher),
            "user-side fetch must find AgentOnBehalfOfUser invocations \
             by user_id even when fetcher has no agent_id"
        );
        // Symmetric.
        assert!(fetcher.matches(&stored));
    }

    #[test]
    fn no_match_pure_agent_to_user() {
        // Pure Agent (no user attribution) vs unrelated user.
        let agent = InvocationOwner::agent(id(1));
        let user = InvocationOwner::user(id(2));
        assert!(!agent.matches(&user));
        assert!(!user.matches(&agent));
    }

    #[test]
    fn no_match_different_agents() {
        let a = InvocationOwner::agent(id(1));
        let b = InvocationOwner::agent(id(2));
        assert!(!a.matches(&b));
    }
}

/// One row from `tool_invocations`. Append-once after creation; all reads
/// hit the same row by id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInvocation {
    pub id: ToolInvocationId,
    pub owner: InvocationOwner,
    pub tool_name: String,
    pub args: serde_json::Value,
    pub args_hash: Vec<u8>,
    pub classifier: String,
    pub summary: serde_json::Value,
    pub raw_text: String,
    pub raw_size_bytes: i64,
    /// The upstream tool's `structured_content` verbatim, when present.
    /// `tool_output_fetch` returns this as the response's
    /// `structured_content` so compose JS callers see the same shape
    /// they'd get from calling the original tool fresh. `None` for
    /// plain-text tools (bash, k8s logs).
    pub original_structured: Option<serde_json::Value>,
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
    pub owner: InvocationOwner,
    pub tool_name: String,
    pub args: serde_json::Value,
    pub args_hash: Vec<u8>,
    pub classifier: String,
    pub summary: serde_json::Value,
    pub raw_text: String,
    pub raw_size_bytes: i64,
    pub original_structured: Option<serde_json::Value>,
    pub exit_code: Option<i32>,
    pub duration_ms: i32,
    pub started_at: DateTime<Utc>,
}
