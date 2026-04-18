use std::sync::Arc;

use async_graphql::{ComplexObject, Context, InputObject, SimpleObject};

use super::agent::Agent;
use super::primitives::*;

use galoy_agents_core::agent::AgentRole as DomainAgentRole;
use galoy_agents_core::workspace::Workspace as DomainWorkspace;

#[derive(SimpleObject, Clone)]
#[graphql(complex)]
pub struct Workspace {
    id: WorkspaceId,
    name: String,
    description: Option<String>,

    #[graphql(skip)]
    pub(super) entity: Arc<DomainWorkspace>,
}

#[ComplexObject]
impl Workspace {
    async fn created_at(&self) -> Timestamp {
        self.entity.created_at().into()
    }

    /// The workspace lead agent.
    async fn lead(&self, ctx: &Context<'_>) -> async_graphql::Result<Option<Agent>> {
        let (app, _sub) = app_and_sub_from_ctx!(ctx);
        let agents = app.agents().list_for_workspace(self.id).await?;
        let lead = agents
            .into_iter()
            .find(|a| a.agent_role == DomainAgentRole::WorkspaceLead)
            .map(Agent::from);
        Ok(lead)
    }
}

impl From<DomainWorkspace> for Workspace {
    fn from(entity: DomainWorkspace) -> Self {
        Self {
            id: entity.id,
            name: entity.name.clone(),
            description: entity.description.clone(),
            entity: Arc::new(entity),
        }
    }
}

#[derive(InputObject)]
pub struct WorkspaceCreateInput {
    pub name: String,
    pub description: Option<String>,
}

mutation_payload! { WorkspaceCreatePayload, workspace: Workspace }

#[derive(InputObject)]
pub struct WorkspaceUpdateInput {
    pub id: WorkspaceId,
    pub name: String,
    pub description: Option<String>,
}

mutation_payload! { WorkspaceUpdatePayload, workspace: Workspace }

#[derive(InputObject)]
pub struct WorkspaceDeleteInput {
    pub id: WorkspaceId,
}

mutation_payload! { WorkspaceDeletePayload, workspace: Workspace }
