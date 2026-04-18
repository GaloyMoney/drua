use async_graphql::{Context, Object};

use super::primitives::*;
use super::workspace::Workspace;

pub struct Query;

#[Object]
impl Query {
    async fn ping(&self) -> &str {
        "pong"
    }

    /// The ID of the currently authenticated user, if any.
    async fn me(&self, ctx: &Context<'_>) -> async_graphql::Result<Option<UUID>> {
        let (_app, sub) = app_and_sub_from_ctx!(ctx);
        match sub.originating_user_id() {
            Some(id) => Ok(Some(UUID::from(uuid::Uuid::from(id)))),
            None => Ok(None),
        }
    }

    async fn workspace(
        &self,
        ctx: &Context<'_>,
        id: WorkspaceId,
    ) -> async_graphql::Result<Option<Workspace>> {
        let (app, _sub) = app_and_sub_from_ctx!(ctx);
        match app.workspaces().find_by_id(id).await {
            Ok(ws) => Ok(Some(Workspace::from(ws))),
            Err(galoy_agents_core::workspace::WorkspaceError::Find(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn workspaces(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<Workspace>> {
        let (app, _sub) = app_and_sub_from_ctx!(ctx);
        let list = app.workspaces().list_all().await?;
        Ok(list.into_iter().map(Workspace::from).collect())
    }
}
