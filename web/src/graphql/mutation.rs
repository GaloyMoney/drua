use async_graphql::{Context, Object};

use super::agent::*;
use super::workspace::*;

use galoy_agents_core::primitives::ChatOutputEvent;

pub struct Mutation;

#[Object]
impl Mutation {
    /// Placeholder — will be replaced with real mutations.
    async fn ping(&self) -> &str {
        "pong"
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

    /// Send a message to an agent and collect the full response.
    async fn agent_send_message(
        &self,
        ctx: &Context<'_>,
        input: AgentSendMessageInput,
    ) -> async_graphql::Result<AgentSendMessagePayload> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let mut rx = app
            .agents()
            .send_message(sub.clone(), input.agent_id, input.message)
            .await?;

        let mut response_text = String::new();
        while let Some(event) = rx.recv().await {
            match event {
                ChatOutputEvent::AssistantText { text } => {
                    response_text.push_str(&text);
                }
                ChatOutputEvent::Error { message } => {
                    return Err(async_graphql::Error::new(message));
                }
                _ => {}
            }
        }

        Ok(AgentSendMessagePayload { response_text })
    }
}
