//! Strongly-typed IDs and authentication primitives shared across the core
//! domain. Folded back into core when the standalone `primitives` and
//! `agent` crates were dissolved.

pub mod auth_subject;
pub mod ids;

pub use auth_subject::{AuthSubject, UserMessageSource};
pub use ids::*;

es_entity::entity_id! {
    WorkspaceSecretId
}

/// Who owns a set of MCP credentials — either a human user or an internal agent.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpCredsOwner {
    User { user_id: UserId },
    Agent { agent_id: AgentId },
}

impl McpCredsOwner {
    pub fn id(&self) -> McpCredsOwnerId {
        match self {
            McpCredsOwner::User { user_id } => McpCredsOwnerId::from(*user_id),
            McpCredsOwner::Agent { agent_id } => McpCredsOwnerId::from(*agent_id),
        }
    }

    pub fn user_id(&self) -> Option<UserId> {
        match self {
            McpCredsOwner::User { user_id } => Some(*user_id),
            McpCredsOwner::Agent { .. } => None,
        }
    }
}

impl From<UserId> for McpCredsOwner {
    fn from(user_id: UserId) -> Self {
        McpCredsOwner::User { user_id }
    }
}

impl From<AgentId> for McpCredsOwner {
    fn from(agent_id: AgentId) -> Self {
        McpCredsOwner::Agent { agent_id }
    }
}
