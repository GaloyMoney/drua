use async_graphql::{Context, Object};

use super::workspace::*;

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
}
