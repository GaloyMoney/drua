use sqlx::PgPool;

use es_entity::*;

use crate::primitives::*;

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "Workspace",
    columns(name(ty = "String")),
    delete = "soft_without_queries"
)]
pub struct WorkspaceRepo {
    #[allow(dead_code)]
    pool: PgPool,
}

impl WorkspaceRepo {
    pub fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }

    pub async fn list_all(&self) -> Result<Vec<Workspace>, WorkspaceQueryError> {
        let (entities, _) = es_query!(
            entity = Workspace,
            "SELECT id, created_at FROM workspaces WHERE NOT deleted ORDER BY created_at DESC LIMIT $1",
            101_i64,
        )
        .fetch_n(&self.pool, 100)
        .await?;
        Ok(entities)
    }
}
