use sqlx::PgPool;

use es_entity::*;

use crate::primitives::*;

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "Agent",
    columns(workspace_id(ty = "WorkspaceId", list_for(by(created_at)))),
    delete = "soft_without_queries"
)]
pub struct AgentRepo {
    #[allow(dead_code)]
    pool: PgPool,
}

impl AgentRepo {
    pub fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }

    /// Find an agent whose UUID starts with the given prefix (e.g. first 8 chars).
    pub async fn find_by_id_prefix(&self, prefix: &str) -> Result<Agent, AgentFindError> {
        let pattern = format!("{prefix}%");
        let row: (uuid::Uuid,) =
            sqlx::query_as("SELECT id FROM agents WHERE NOT deleted AND id::text LIKE $1 LIMIT 1")
                .bind(&pattern)
                .fetch_optional(&self.pool)
                .await?
                .ok_or(AgentFindError::NotFound {
                    entity: "Agent",
                    column: None,
                    value: prefix.to_string(),
                })?;
        self.find_by_id(AgentId::from(row.0)).await
    }
}
