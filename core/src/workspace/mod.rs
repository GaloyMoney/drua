mod entity;
pub mod error;
pub(crate) mod repo;

use std::sync::Arc;

use tracing::instrument;

pub use entity::Workspace;
use entity::*;
pub use error::*;
use repo::*;

use crate::agent::Agents;
use crate::mcp_creds::McpCredentials;
use crate::primitives::*;

#[derive(Clone)]
pub struct Workspaces {
    repo: WorkspaceRepo,
    agents: Arc<Agents>,
    mcp_creds: McpCredentials,
}

impl Workspaces {
    pub fn new(pool: &sqlx::PgPool, agents: Arc<Agents>, mcp_creds: McpCredentials) -> Self {
        let repo = WorkspaceRepo::new(pool);
        Self {
            repo,
            agents,
            mcp_creds,
        }
    }

    #[instrument(name = "domain.workspace.create", skip(self))]
    pub async fn create(
        &self,
        user_id: UserId,
        name: impl Into<String> + std::fmt::Debug,
        description: Option<String>,
    ) -> Result<Workspace, WorkspaceError> {
        let name = name.into();
        let new_workspace = build_new_workspace(&name, description);
        let workspace_id = new_workspace.id;

        let mut op = self.repo.begin_op().await?;
        let workspace = self.repo.create_in_op(&mut op, new_workspace).await?;

        self.agents
            .create_in_op(
                &mut op,
                workspace_id,
                AgentType::WorkspaceLead,
                user_id,
                format!("{name}-lead"),
            )
            .await?;

        op.commit().await?;
        Ok(workspace)
    }

    #[instrument(name = "domain.workspace.find_by_id", skip(self))]
    pub async fn find_by_id(
        &self,
        id: impl Into<WorkspaceId> + std::fmt::Debug,
    ) -> Result<Workspace, WorkspaceError> {
        Ok(self.repo.find_by_id(id.into()).await?)
    }

    #[instrument(name = "domain.workspace.list_all", skip(self))]
    pub async fn list_all(&self) -> Result<Vec<Workspace>, WorkspaceError> {
        Ok(self.repo.list_all().await?)
    }

    #[instrument(name = "domain.workspace.update", skip(self))]
    pub async fn update(
        &self,
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
        id: impl Into<WorkspaceId> + std::fmt::Debug,
    ) -> Result<Workspace, WorkspaceError> {
        let mut op = self.repo.begin_op().await?;
        let workspace = self.delete_in_op(&mut op, id).await?;
        op.commit().await?;
        Ok(workspace)
    }

    #[instrument(name = "domain.workspace.delete_in_op", skip(self, op))]
    pub async fn delete_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        id: impl Into<WorkspaceId> + std::fmt::Debug,
    ) -> Result<Workspace, WorkspaceError> {
        let id = id.into();
        let mut workspace = self.repo.find_by_id(id).await?;

        if !workspace.archive().did_execute() {
            return Ok(workspace);
        }

        // Cascade: revoke MCP creds and tear down sandboxes for all agents
        let agent_list = self.agents.list_for_workspace(id).await?;
        for agent in &agent_list {
            // Revoke gateway credentials so the agent can no longer call MCP
            if let Err(e) = self.mcp_creds.revoke_in_op(op, agent.mcp_creds_id).await {
                tracing::warn!(
                    agent_id = %agent.id,
                    creds_id = %agent.mcp_creds_id,
                    error = %e,
                    "Failed to revoke agent MCP creds during workspace delete"
                );
            }

            // Request sandbox teardown (best-effort, non-blocking)
            if let Err(e) = self.agents.destroy_sandbox(agent.id).await {
                tracing::warn!(
                    agent_id = %agent.id,
                    error = %e,
                    "Failed to destroy agent sandbox during workspace delete"
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

fn build_new_workspace(name: impl Into<String>, description: Option<String>) -> NewWorkspace {
    let mut builder = NewWorkspace::builder();
    builder.name(name);
    if let Some(desc) = description {
        builder.description(desc);
    }
    builder.build().expect("Could not build new workspace")
}
