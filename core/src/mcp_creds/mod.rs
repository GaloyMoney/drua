mod entity;
pub mod error;
pub mod repo;
pub mod token;

use tracing::instrument;

pub use entity::McpCreds;
use entity::*;
pub use error::*;
use repo::*;

use crate::primitives::*;

#[derive(Clone)]
pub struct McpCredentials {
    repo: McpCredsRepo,
}

impl McpCredentials {
    pub fn new(pool: &sqlx::PgPool) -> Self {
        let repo = McpCredsRepo::new(pool);
        Self { repo }
    }

    #[instrument(name = "domain.mcp_creds.create_for_user", skip(self))]
    pub async fn create_for_user(
        &self,
        user_id: UserId,
        name: impl Into<String> + std::fmt::Debug,
        token_hash: impl Into<String> + std::fmt::Debug,
        scopes: Vec<String>,
    ) -> Result<McpCreds, McpCredsError> {
        let new_creds = NewMcpCreds::builder()
            .user_id(user_id)
            .name(name)
            .token_hash(token_hash)
            .scopes(scopes)
            .build()
            .expect("Could not build new mcp creds");

        let creds = self.repo.create(new_creds).await?;

        Ok(creds)
    }

    #[instrument(name = "domain.mcp_creds.create_for_user_in_op", skip(self, op))]
    pub async fn create_for_user_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        user_id: UserId,
        name: impl Into<String> + std::fmt::Debug,
        token_hash: impl Into<String> + std::fmt::Debug,
        scopes: Vec<String>,
    ) -> Result<McpCreds, McpCredsError> {
        let new_creds = NewMcpCreds::builder()
            .user_id(user_id)
            .name(name)
            .token_hash(token_hash)
            .scopes(scopes)
            .build()
            .expect("Could not build new mcp creds");

        let creds = self.repo.create_in_op(op, new_creds).await?;

        Ok(creds)
    }

    #[instrument(name = "domain.mcp_creds.revoke", skip(self))]
    pub async fn revoke(
        &self,
        user_id: UserId,
        id: impl Into<McpCredsId> + std::fmt::Debug,
    ) -> Result<McpCreds, McpCredsError> {
        let id = id.into();
        let mut creds = self.repo.find_by_id(id).await?;

        if creds.user_id != user_id {
            return Err(McpCredsError::AuthorizationError);
        }

        if creds.revoke().did_execute() {
            self.repo.update(&mut creds).await?;
        }

        Ok(creds)
    }

    #[instrument(name = "domain.mcp_creds.list_for_user", skip(self))]
    pub async fn list_for_user(
        &self,
        user_id: UserId,
        query: es_entity::PaginatedQueryArgs<repo::mcp_creds_cursor::McpCredsByCreatedAtCursor>,
        direction: es_entity::ListDirection,
    ) -> Result<
        es_entity::PaginatedQueryRet<McpCreds, repo::mcp_creds_cursor::McpCredsByCreatedAtCursor>,
        McpCredsError,
    > {
        Ok(self
            .repo
            .list_for_user_id_by_created_at(user_id, query, direction)
            .await?)
    }

    #[instrument(name = "domain.mcp_creds.list_all_for_user", skip(self))]
    pub async fn list_all_for_user(&self, user_id: UserId) -> Result<Vec<McpCreds>, McpCredsError> {
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

    #[instrument(name = "domain.mcp_creds.find_by_id", skip(self))]
    pub async fn find_by_id(
        &self,
        id: impl Into<McpCredsId> + std::fmt::Debug,
    ) -> Result<McpCreds, McpCredsError> {
        Ok(self.repo.find_by_id(id.into()).await?)
    }

    #[instrument(name = "domain.mcp_creds.find_by_token_hash", skip(self))]
    pub async fn find_by_token_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<McpCreds>, McpCredsError> {
        Ok(self.repo.maybe_find_by_token_hash(token_hash).await?)
    }
}
