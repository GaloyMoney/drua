use std::sync::Arc;

use async_graphql::{ComplexObject, Context, InputObject, SimpleObject};

use super::agent::Agent;
use super::primitives::*;

use drua_core::workspace::Workspace as DomainWorkspace;

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
    async fn lead(&self, ctx: &Context<'_>) -> async_graphql::Result<Agent> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let agent = app
            .agents()
            .find_by_id(sub, self.entity.lead_agent_id)
            .await?;
        Ok(Agent::from(agent))
    }

    /// All agents in this workspace (lead agent first).
    async fn agents(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<Agent>> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let mut agents = app.agents().list_for_workspace(sub, self.entity.id).await?;
        let lead_id = self.entity.lead_agent_id;
        agents.sort_by_key(|a| if a.id == lead_id { 0 } else { 1 });
        Ok(agents.into_iter().map(Agent::from).collect())
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
