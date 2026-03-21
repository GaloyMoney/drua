use async_graphql::*;

use crate::agent::Agent;
use crate::primitives::*;
use crate::user::User;

#[derive(SimpleObject)]
pub struct UserType {
    id: UserId,
    github_id: String,
    email: Option<String>,
    name: Option<String>,
}

impl From<User> for UserType {
    fn from(user: User) -> Self {
        Self {
            id: user.id,
            github_id: user.github_id.clone(),
            email: user.email.clone(),
            name: user.name.clone(),
        }
    }
}

#[derive(SimpleObject)]
pub struct AgentType {
    id: AgentId,
    name: String,
    scopes: Vec<String>,
    created_at: chrono::DateTime<chrono::Utc>,
    revoked_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl From<Agent> for AgentType {
    fn from(agent: Agent) -> Self {
        Self {
            id: agent.id,
            name: agent.name.clone(),
            scopes: agent.scopes.clone(),
            created_at: agent.created_at(),
            revoked_at: agent.revoked_at,
        }
    }
}

impl From<&Agent> for AgentType {
    fn from(agent: &Agent) -> Self {
        Self {
            id: agent.id,
            name: agent.name.clone(),
            scopes: agent.scopes.clone(),
            created_at: agent.created_at(),
            revoked_at: agent.revoked_at,
        }
    }
}

#[derive(SimpleObject)]
pub struct CreateAgentPayload {
    pub agent: AgentType,
    pub token: String,
}

#[derive(SimpleObject)]
pub struct RevokeAgentPayload {
    pub agent: AgentType,
}
