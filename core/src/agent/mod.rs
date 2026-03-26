mod entity;
pub mod error;
pub mod repo;
pub mod token;

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

    #[instrument(name = "domain.agent.create_for_user", skip(self))]
    pub async fn create_for_user(
        &self,
        user_id: UserId,
        name: impl Into<String> + std::fmt::Debug,
        token_hash: impl Into<String> + std::fmt::Debug,
        scopes: Vec<String>,
    ) -> Result<Agent, AgentError> {
        let new_agent = NewAgent::builder()
            .user_id(user_id)
            .name(name)
            .token_hash(token_hash)
            .scopes(scopes)
            .build()
            .expect("Could not build new agent");

        let agent = self.repo.create(new_agent).await?;

        Ok(agent)
    }

    #[instrument(name = "domain.agent.create_for_user_in_op", skip(self, op))]
    pub async fn create_for_user_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        user_id: UserId,
        name: impl Into<String> + std::fmt::Debug,
        token_hash: impl Into<String> + std::fmt::Debug,
        scopes: Vec<String>,
    ) -> Result<Agent, AgentError> {
        let new_agent = NewAgent::builder()
            .user_id(user_id)
            .name(name)
            .token_hash(token_hash)
            .scopes(scopes)
            .build()
            .expect("Could not build new agent");

        let agent = self.repo.create_in_op(op, new_agent).await?;

        Ok(agent)
    }

    #[instrument(name = "domain.agent.revoke", skip(self))]
    pub async fn revoke(
        &self,
        user_id: UserId,
        id: impl Into<AgentId> + std::fmt::Debug,
    ) -> Result<Agent, AgentError> {
        let id = id.into();
        let mut agent = self.repo.find_by_id(id).await?;

        if agent.user_id != user_id {
            return Err(AgentError::AuthorizationError);
        }

        if agent.revoke().did_execute() {
            self.repo.update(&mut agent).await?;
        }

        Ok(agent)
    }

    #[instrument(name = "domain.agent.list_for_user", skip(self))]
    pub async fn list_for_user(
        &self,
        user_id: UserId,
        query: es_entity::PaginatedQueryArgs<repo::agent_cursor::AgentsByCreatedAtCursor>,
        direction: es_entity::ListDirection,
    ) -> Result<
        es_entity::PaginatedQueryRet<Agent, repo::agent_cursor::AgentsByCreatedAtCursor>,
        AgentError,
    > {
        Ok(self
            .repo
            .list_for_user_id_by_created_at(user_id, query, direction)
            .await?)
    }

    #[instrument(name = "domain.agent.list_all_for_user", skip(self))]
    pub async fn list_all_for_user(&self, user_id: UserId) -> Result<Vec<Agent>, AgentError> {
        let query = es_entity::PaginatedQueryArgs {
            first: 100,
            after: None,
        };
        let result = self
            .repo
            .list_for_user_id_by_created_at(user_id, query, es_entity::ListDirection::Descending)
            .await?;
        Ok(result.entities)
    }

    #[instrument(name = "domain.agent.find_by_id", skip(self))]
    pub async fn find_by_id(
        &self,
        id: impl Into<AgentId> + std::fmt::Debug,
    ) -> Result<Agent, AgentError> {
        Ok(self.repo.find_by_id(id.into()).await?)
    }

    #[instrument(name = "domain.agent.find_by_token_hash", skip(self))]
    pub async fn find_by_token_hash(&self, token_hash: &str) -> Result<Option<Agent>, AgentError> {
        Ok(self.repo.maybe_find_by_token_hash(token_hash).await?)
    }
}
