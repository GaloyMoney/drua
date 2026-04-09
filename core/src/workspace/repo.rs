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

    pub async fn list_all(&self) -> Result<Vec<Workspace>, sqlx::Error> {
        let rows: Vec<(uuid::Uuid,)> =
            // @@ use es_query! to load all entities at the same time
            sqlx::query_as("SELECT id FROM workspaces WHERE NOT deleted ORDER BY created_at DESC")
                .fetch_all(&self.pool)
                .await?;

        let mut workspaces = Vec::with_capacity(rows.len());
        // @@ avoid n + 1
        for (id,) in rows {
            let ws_id = WorkspaceId::from(id);
            match self.find_by_id(ws_id).await {
                Ok(ws) => workspaces.push(ws),
                Err(e) => {
                    tracing::warn!(id = %ws_id, error = %e, "Failed to hydrate workspace, skipping");
                }
            }
        }

        Ok(workspaces)
    }
}
