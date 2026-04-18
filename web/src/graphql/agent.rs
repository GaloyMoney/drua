use std::sync::Arc;

use async_graphql::{Enum, SimpleObject};

use super::primitives::*;

use galoy_agents_core::agent::Agent as DomainAgent;
use galoy_agents_core::agent::AgentRole as DomainAgentRole;

#[derive(SimpleObject, Clone)]
pub struct Agent {
    id: AgentId,
    workspace_id: WorkspaceId,
    name: String,
    role: AgentRole,

    #[graphql(skip)]
    #[allow(dead_code)]
    pub(super) entity: Arc<DomainAgent>,
}

impl From<DomainAgent> for Agent {
    fn from(entity: DomainAgent) -> Self {
        Self {
            id: entity.id,
            workspace_id: entity.workspace_id,
            name: entity.name.clone(),
            role: AgentRole::from(entity.agent_role),
            entity: Arc::new(entity),
        }
    }
}

#[derive(Enum, Clone, Copy, PartialEq, Eq)]
pub enum AgentRole {
    WorkspaceLead,
    Agent,
}

impl From<DomainAgentRole> for AgentRole {
    fn from(role: DomainAgentRole) -> Self {
        match role {
            DomainAgentRole::WorkspaceLead => Self::WorkspaceLead,
            DomainAgentRole::Agent => Self::Agent,
        }
    }
}
