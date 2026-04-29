use async_graphql::{
    types::connection::{Connection, EmptyFields},
    Context, Object, SimpleObject,
};

use super::agent::Agent;
use super::audit::{AuditEntry, AuditLogQueryInput};
use super::library::{LibrarySearchHit, LibrarySearchInput};
use super::primitives::*;
use super::sandbox::Sandbox;
use super::workspace::Workspace;
use super::AppConfigYaml;

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

    async fn app_config(&self, ctx: &Context<'_>) -> Yaml {
        ctx.data_unchecked::<AppConfigYaml>().0.clone()
    }

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

    /// Pi-compatible JSONL (v3). When `thread_id` is omitted the current
    /// main thread is exported.
    async fn export_thread(
        &self,
        ctx: &Context<'_>,
        agent_id: AgentId,
        thread_id: Option<SessionThreadId>,
    ) -> async_graphql::Result<String> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let jsonl = app.agents().export_thread(sub, agent_id, thread_id).await?;
        Ok(jsonl)
    }

    async fn sandbox(
        &self,
        ctx: &Context<'_>,
        id: SandboxId,
    ) -> async_graphql::Result<Option<Sandbox>> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        match app.sandboxes().find_by_id(sub, id).await {
            Ok(sb) => Ok(Some(Sandbox::from(sb))),
            Err(drua_core::sandbox::SandboxError::Find(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn audit_log(
        &self,
        ctx: &Context<'_>,
        input: AuditLogQueryInput,
    ) -> async_graphql::Result<Vec<AuditEntry>> {
        let (app, _sub) = app_and_sub_from_ctx!(ctx);
        let query = drua_core::audit::AuditLogQuery {
            workspace_id: input.workspace_id,
            acting_agent_id: input.acting_agent_id,
            action: input.action,
            outcome: input.outcome,
            error: input.error,
            limit: input.limit.clamp(1, 100) as i64,
            ..Default::default()
        };
        let entries = app.audit().find(&query).await?;
        Ok(entries.into_iter().map(AuditEntry::from).collect())
    }

    /// Cross-type, cross-workspace library search. Open to any
    /// authenticated subject; library content is globally discoverable.
    /// `workspaceId` is optional — when set the workspace is validated
    /// via `find_by_id`; when omitted no workspace filter is applied.
    async fn library_search(
        &self,
        ctx: &Context<'_>,
        input: LibrarySearchInput,
    ) -> async_graphql::Result<Vec<LibrarySearchHit>> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);

        let doc_types: Vec<drua_core::library::DocType> = input
            .types
            .unwrap_or_default()
            .into_iter()
            .map(Into::into)
            .collect();
        let limit = input.limit.clamp(1, 200) as usize;

        let workspace_ids: Vec<uuid::Uuid> = match input.workspace_id {
            Some(id) => {
                app.workspaces().find_by_id(sub, id).await?;
                vec![uuid::Uuid::from(id)]
            }
            None => Vec::new(),
        };

        let hits = app
            .library()
            .search_global(sub, &workspace_ids, &input.query, &doc_types, limit)
            .await?;

        Ok(hits
            .into_iter()
            .map(LibrarySearchHit::from_domain)
            .collect())
    }
}
