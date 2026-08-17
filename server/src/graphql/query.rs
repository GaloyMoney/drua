use async_graphql::{
    types::connection::{Connection, EmptyFields},
    Context, Object, SimpleObject,
};

use super::agent::Agent;
use super::audit::{AuditEntry, AuditLogQueryInput};
use super::library::{LibrarySearchHit, LibrarySearchInput};
use super::primitives::*;
use super::project::Project;
use super::sandbox::Sandbox;
use super::workflow::{limit, WorkflowDefinition, WorkflowRun};

use drua_core::project::ProjectByCreatedAtCursor;

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

    async fn app_config(&self, ctx: &Context<'_>) -> async_graphql::Result<Yaml> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        Ok(app.app_config(sub)?.into())
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

    async fn project(
        &self,
        ctx: &Context<'_>,
        id: ProjectId,
    ) -> async_graphql::Result<Option<Project>> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        match app.projects().find_by_id(sub, id).await {
            Ok(project) => Ok(Some(Project::from(project))),
            Err(drua_core::project::ProjectError::Find(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn workflow_definitions(
        &self,
        ctx: &Context<'_>,
        project_id: ProjectId,
    ) -> async_graphql::Result<Vec<WorkflowDefinition>> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let definitions = app.workflows().list_for_project(sub, project_id).await?;
        Ok(definitions
            .into_iter()
            .map(WorkflowDefinition::from)
            .collect())
    }

    async fn workflow_definition(
        &self,
        ctx: &Context<'_>,
        id: WorkflowDefinitionId,
    ) -> async_graphql::Result<Option<WorkflowDefinition>> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        match app.workflows().find_by_id(sub, id).await {
            Ok(definition) => Ok(Some(WorkflowDefinition::from(definition))),
            Err(drua_core::workflow::WorkflowError::DefinitionFind(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn workflow_runs(
        &self,
        ctx: &Context<'_>,
        definition_id: WorkflowDefinitionId,
        #[graphql(default = 50)] first: i32,
    ) -> async_graphql::Result<Vec<WorkflowRun>> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let runs = app.workflows().list_runs(sub, definition_id).await?;
        Ok(limit(runs, first)
            .into_iter()
            .map(WorkflowRun::from)
            .collect())
    }

    async fn workflow_run(
        &self,
        ctx: &Context<'_>,
        id: WorkflowRunId,
    ) -> async_graphql::Result<Option<WorkflowRun>> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        match app.workflows().find_run_by_id(sub, id).await {
            Ok(run) => Ok(Some(WorkflowRun::from(run))),
            Err(drua_core::workflow::WorkflowError::RunFind(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn projects(
        &self,
        ctx: &Context<'_>,
        first: i32,
        after: Option<String>,
    ) -> async_graphql::Result<
        Connection<ProjectByCreatedAtCursor, Project, EmptyFields, EmptyFields>,
    > {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        list_with_cursor!(ProjectByCreatedAtCursor, Project, after, first, |query| app
            .projects()
            .list(sub, query, es_entity::ListDirection::Descending))
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
            project_id: input.project_id,
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

    /// Cross-type, cross-project library search. Open to any
    /// authenticated subject; library content is globally discoverable.
    /// `projectId` is optional — when set the project is validated
    /// via `find_by_id`; when omitted no project filter is applied.
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

        // When `projectId` is set, also include the project's mounted
        // space ids in the scope filter — agents in that project can
        // see content from those spaces, so a project-scoped search
        // must surface them too. Without this, `library_search` and
        // `notes.search` silently hide space-scoped content from any
        // project-filtered query.
        let project_ids: Vec<uuid::Uuid> = match input.project_id {
            Some(id) => {
                app.projects().find_by_id(sub, id).await?;
                let mut ids = Vec::with_capacity(1);
                ids.push(uuid::Uuid::from(id));
                match app.notes().space_mounts().space_ids_for_project(id).await {
                    Ok(mounted) => ids.extend(mounted.into_iter().map(uuid::Uuid::from)),
                    Err(e) => {
                        tracing::warn!(error = %e, "space_ids_for_project failed; project-only search");
                    }
                }
                ids
            }
            None => Vec::new(),
        };

        let hits = app
            .search()
            .search(sub, &project_ids, &input.query, &doc_types, &[], limit)
            .await?;

        Ok(hits
            .into_iter()
            .map(LibrarySearchHit::from_domain)
            .collect())
    }
}
