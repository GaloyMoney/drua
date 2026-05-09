use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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

    // cursor #3212630890: AgentOnBehalfOfUser populates both fields so the
    // same user can re-fetch later via ExportedAgent.
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
        let stored = InvocationOwner {
            agent_id: Some(id(1)),
            user_id: Some(id(2)),
        };
        let fetcher = InvocationOwner::user(id(2));
        assert!(stored.matches(&fetcher));
        assert!(fetcher.matches(&stored));
    }

    #[test]
    fn no_match_pure_agent_to_user() {
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
    pub original_structured: Option<serde_json::Value>,
    pub exit_code: Option<i32>,
    pub duration_ms: i32,
    pub started_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

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
