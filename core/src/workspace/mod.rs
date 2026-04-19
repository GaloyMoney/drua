mod entity;
pub mod error;
pub(crate) mod repo;

use std::sync::Arc;

use es_entity::*;
use tracing::instrument;

pub use crate::workspace::workspace_cursor::WorkspaceByCreatedAtCursor;
pub use entity::Workspace;
use entity::*;
pub use error::*;
use repo::*;

use crate::agent::{AgentRole, Agents};
use crate::primitives::*;

#[derive(Clone)]
pub struct Workspaces {
    repo: WorkspaceRepo,
    agents: Arc<Agents>,
}

impl Workspaces {
    pub fn new(pool: &sqlx::PgPool, agents: Arc<Agents>) -> Self {
        let repo = WorkspaceRepo::new(pool);
        Self { repo, agents }
    }

    #[instrument(name = "domain.workspace.create", skip(self))]
    pub async fn create(
        &self,
        sub: &AuthSubject,
        name: impl Into<String> + std::fmt::Debug,
        description: Option<String>,
    ) -> Result<Workspace, WorkspaceError> {
        let name = name.into();
        let lead_agent_id = AgentId::new();
        let new_workspace = build_new_workspace(lead_agent_id, &name, description);
        let workspace_id = new_workspace.id;

        let mut op = self.repo.begin_op().await?;
        let workspace = self.repo.create_in_op(&mut op, new_workspace).await?;

        self.agents
            .create_in_op(
                &mut op,
                lead_agent_id,
                workspace_id,
                AgentRole::WorkspaceLead,
                "lead",
                None,
            )
            .await?;

        op.commit().await?;

        tracing::info!(
            workspace.id = %workspace.id,
            sub = ?sub,
            "workspace created"
        );

        Ok(workspace)
    }

    #[instrument(name = "domain.workspace.find_by_id", skip(self, _sub))]
    pub async fn find_by_id(
        &self,
        _sub: &AuthSubject,
        id: impl Into<WorkspaceId> + std::fmt::Debug,
    ) -> Result<Workspace, WorkspaceError> {
        Ok(self.repo.find_by_id(id.into()).await?)
    }

    #[instrument(name = "domain.workspace.list", skip(self))]
    pub async fn list(
        &self,
        _sub: &AuthSubject,
        query: PaginatedQueryArgs<WorkspaceByCreatedAtCursor>,
        direction: ListDirection,
    ) -> Result<PaginatedQueryRet<Workspace, WorkspaceByCreatedAtCursor>, WorkspaceError> {
        Ok(self.repo.list_by_created_at(query, direction).await?)
    }

    #[instrument(name = "domain.workspace.list_all", skip(self, _sub))]
    pub async fn list_all(&self, _sub: &AuthSubject) -> Result<Vec<Workspace>, WorkspaceError> {
        Ok(self.repo.list_all().await?)
    }

    #[instrument(name = "domain.workspace.update", skip(self))]
    pub async fn update(
        &self,
        _sub: &AuthSubject,
        id: impl Into<WorkspaceId> + std::fmt::Debug,
        name: impl Into<String> + std::fmt::Debug,
        description: Option<String>,
    ) -> Result<Workspace, WorkspaceError> {
        let mut workspace = self.repo.find_by_id(id.into()).await?;
        workspace.update(name, description);
        self.repo.update(&mut workspace).await?;
        Ok(workspace)
    }

    #[instrument(name = "domain.workspace.delete", skip(self))]
    pub async fn delete(
        &self,
        sub: &AuthSubject,
        id: impl Into<WorkspaceId> + std::fmt::Debug,
    ) -> Result<Workspace, WorkspaceError> {
        let mut op = self.repo.begin_op().await?;
        let workspace = self.delete_in_op(&mut op, sub, id).await?;
        op.commit().await?;
        Ok(workspace)
    }

    #[instrument(name = "domain.workspace.delete_in_op", skip(self, op, _sub))]
    pub async fn delete_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        _sub: &AuthSubject,
        id: impl Into<WorkspaceId> + std::fmt::Debug,
    ) -> Result<Workspace, WorkspaceError> {
        let id = id.into();
        let mut workspace = self.repo.find_by_id(id).await?;

        if !workspace.archive().did_execute() {
            return Ok(workspace);
        }

        // Cascade: soft-delete agents (which also destroys their sandboxes)
        let agent_list = self.agents.list_for_workspace(_sub, id).await?;
        for agent in &agent_list {
            if let Err(e) = self.agents.delete_in_op(op, agent.id).await {
                tracing::warn!(
                    agent_id = %agent.id,
                    error = %e,
                    "Failed to delete agent during workspace delete"
                );
            }
        }

        self.repo.update_in_op(op, &mut workspace).await?;
        self.repo
            .delete_in_op(op, self.repo.find_by_id(id).await?)
            .await?;
        Ok(workspace)
    }
}

fn build_new_workspace(
    lead_agent_id: AgentId,
    name: impl Into<String>,
    description: Option<String>,
) -> NewWorkspace {
    let mut builder = NewWorkspace::builder();
    builder.lead_agent_id(lead_agent_id);
    builder.name(name);
    if let Some(desc) = description {
        builder.description(desc);
    }
    builder.build().expect("Could not build new workspace")
}
