use async_graphql::{
    types::connection::{Connection, EmptyFields},
    Context, Object, SimpleObject,
};

use super::primitives::*;
use super::workspace::Workspace;

use galoy_agents_core::workspace::WorkspaceByCreatedAtCursor;

#[derive(SimpleObject)]
pub struct Me {
    id: UUID,
    name: Option<String>,
    email: Option<String>,
    github_username: Option<String>,
}

pub struct Query;

#[Object]
impl Query {
    async fn ping(&self) -> &str {
        "pong"
    }

    /// The currently authenticated user, if any.
    async fn me(&self, ctx: &Context<'_>) -> async_graphql::Result<Option<Me>> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        match sub.originating_user_id() {
            Some(id) => {
                let user = app.users().find_by_id(id).await?;
                Ok(Some(Me {
                    id: UUID::from(uuid::Uuid::from(id)),
                    name: user.name,
                    email: user.email,
                    github_username: user.github_username,
                }))
            }
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
