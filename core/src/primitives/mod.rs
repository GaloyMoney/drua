//! Strongly-typed IDs and authentication primitives shared across the core
//! domain. Folded back into core when the standalone `primitives` and
//! `agent` crates were dissolved.

mod chat_output_event;
mod context_generation;

pub use crate::auth::{error::AuthorizationError, AuthResource, AuthScope, AuthSubject, AuthVerb};
pub use chat_output_event::ChatOutputEvent;
pub use context_generation::{ContextBumpHook, ContextGeneration, ScopeId};

// Re-export SpaceId from drua_library so there's a single canonical type.
pub use drua_library::SpaceId;

es_entity::entity_id! {
    UserId,
    AgentId,
    ProjectId,
    McpCredsId,
    McpCredsOwnerId,
    SandboxId,
    ProjectSecretId,
    SkillId,
    NoteId,
    ToolInvocationId,
    WorkflowDefinitionId,
    WorkflowRunId;

    UserId => McpCredsOwnerId,
    AgentId => McpCredsOwnerId,
    WorkflowRunId => job::JobId,
}

impl WorkflowRunId {
    /// First UUID segment (8 hex chars). Used to build human-readable
    /// agent / sandbox names that stay readable while still being
    /// unique enough within a project.
    pub fn short(&self) -> String {
        let s = self.to_string();
        s.split_once('-').map(|(p, _)| p.to_string()).unwrap_or(s)
    }
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

/// Originator of a user-facing message sent to an agent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserMessageSource {
    User {
        user_id: UserId,
    },
    ExportedAgent {
        user_id: UserId,
        creds_id: McpCredsId,
    },
    Agent {
        agent_id: AgentId,
    },
}
