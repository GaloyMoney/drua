use async_graphql::{
    types::connection::{Connection, EmptyFields},
    Context, Object, SimpleObject,
};

use super::agent::Agent;
use super::primitives::*;
use super::workspace::Workspace;

use drua_core::workspace::WorkspaceByCreatedAtCursor;

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

    /// Look up a single agent by ID.
    async fn agent(&self, ctx: &Context<'_>, id: AgentId) -> async_graphql::Result<Option<Agent>> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        match app.agents().find_by_id(sub, id).await {
            Ok(agent) => Ok(Some(Agent::from(agent))),
            Err(drua_core::agent::AgentError::Find(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn workspace(
        &self,
        ctx: &Context<'_>,
        id: WorkspaceId,
    ) -> async_graphql::Result<Option<Workspace>> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        match app.workspaces().find_by_id(sub, id).await {
            Ok(ws) => Ok(Some(Workspace::from(ws))),
            Err(drua_core::workspace::WorkspaceError::Find(_)) => Ok(None),
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

    /// Export an agent's current thread as Pi-compatible JSONL (v3 format).
    async fn export_thread(
        &self,
        ctx: &Context<'_>,
        agent_id: AgentId,
    ) -> async_graphql::Result<String> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let jsonl = app.agents().export_thread(sub, agent_id).await?;
        Ok(jsonl)
    }
}
