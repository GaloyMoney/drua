use async_graphql::{Context, InputObject, Object};

use super::agent::Agent;
use super::primitives::*;
use super::workspace::*;

#[derive(InputObject)]
pub struct AgentCreateInput {
    pub workspace_id: WorkspaceId,
    pub name: String,
}

mutation_payload! { AgentCreatePayload, agent: Agent }

pub struct Mutation;

#[Object]
impl Mutation {
    async fn ping(&self) -> &str {
        "pong"
    }

    async fn agent_create(
        &self,
        ctx: &Context<'_>,
        input: AgentCreateInput,
    ) -> async_graphql::Result<AgentCreatePayload> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let agent = app
            .agents()
            .create_agent(sub, input.workspace_id, input.name, None)
            .await?;
        Ok(AgentCreatePayload::from(Agent::from(agent)))
    }

    async fn workspace_create(
        &self,
        ctx: &Context<'_>,
        input: WorkspaceCreateInput,
    ) -> async_graphql::Result<WorkspaceCreatePayload> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let ws = app
            .workspaces()
            .create(sub, input.name, input.description)
            .await?;
        Ok(WorkspaceCreatePayload::from(Workspace::from(ws)))
    }

    async fn workspace_update(
        &self,
        ctx: &Context<'_>,
        input: WorkspaceUpdateInput,
    ) -> async_graphql::Result<WorkspaceUpdatePayload> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let ws = app
            .workspaces()
            .update(sub, input.id, input.name, input.description)
            .await?;
        Ok(WorkspaceUpdatePayload::from(Workspace::from(ws)))
    }

    async fn workspace_delete(
        &self,
        ctx: &Context<'_>,
        input: WorkspaceDeleteInput,
    ) -> async_graphql::Result<WorkspaceDeletePayload> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let ws = app.workspaces().delete(sub, input.id).await?;
        Ok(WorkspaceDeletePayload::from(Workspace::from(ws)))
    }
}
