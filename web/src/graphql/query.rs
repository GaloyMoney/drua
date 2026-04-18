use async_graphql::{
    types::connection::{Connection, EmptyFields},
    Context, Object,
};

use super::primitives::*;
use super::workspace::Workspace;

use galoy_agents_core::workspace::WorkspaceByCreatedAtCursor;

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

    async fn workspaces(
        &self,
        ctx: &Context<'_>,
        first: i32,
        after: Option<String>,
    ) -> async_graphql::Result<
        Connection<WorkspaceByCreatedAtCursor, Workspace, EmptyFields, EmptyFields>,
    > {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        list_with_cursor!(
            WorkspaceByCreatedAtCursor,
            Workspace,
            after,
            first,
            |query| app
                .workspaces()
                .list(sub, query, es_entity::ListDirection::Descending)
        )
    }
}
