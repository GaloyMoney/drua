mod entity;
pub mod error;
pub(crate) mod repo;

use tracing::instrument;

pub use entity::Agent;
use entity::*;
pub use error::*;
use repo::*;

use crate::primitives::*;

#[derive(Clone)]
pub struct Agents {
    repo: AgentRepo,
}

impl Agents {
    pub fn new(pool: &sqlx::PgPool) -> Self {
        let repo = AgentRepo::new(pool);
        Self { repo }
    }

    #[instrument(name = "domain.agent.create", skip(self))]
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        agent_type: AgentType,
        name: impl Into<String> + std::fmt::Debug,
    ) -> Result<Agent, AgentError> {
        let mut op = self.repo.begin_op().await?;
        let agent = self
            .create_in_op(&mut op, workspace_id, agent_type, name)
            .await?;
        op.commit().await?;
        Ok(agent)
    }

    #[instrument(name = "domain.agent.create_in_op", skip(self, op))]
    pub async fn create_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        workspace_id: WorkspaceId,
        agent_type: AgentType,
        name: impl Into<String> + std::fmt::Debug,
    ) -> Result<Agent, AgentError> {
        let new_agent = NewAgent::builder()
            .workspace_id(workspace_id)
            .agent_type(agent_type)
            .name(name)
            .build()
            .expect("Could not build new agent");

        let agent = self.repo.create_in_op(op, new_agent).await?;
        Ok(agent)
    }

    #[instrument(name = "domain.agent.find_by_id", skip(self))]
    pub async fn find_by_id(
        &self,
        id: impl Into<AgentId> + std::fmt::Debug,
    ) -> Result<Agent, AgentError> {
        Ok(self.repo.find_by_id(id.into()).await?)
    }

    #[instrument(name = "domain.agent.list_for_workspace", skip(self))]
    pub async fn list_for_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<Agent>, AgentError> {
        let query = es_entity::PaginatedQueryArgs {
            first: 100,
            after: None,
        };
        let result = self
            .repo
            .list_for_workspace_id_by_created_at(
                workspace_id,
                query,
                es_entity::ListDirection::Descending,
            )
            .await?;
        Ok(result.entities)
    }
}
