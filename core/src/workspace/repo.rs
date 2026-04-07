use sqlx::PgPool;

use es_entity::*;

use crate::primitives::*;

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(entity = "Workspace", columns(name(ty = "String")))]
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
            sqlx::query_as("SELECT id FROM workspaces ORDER BY created_at DESC")
                .fetch_all(&self.pool)
                .await?;

        let mut workspaces = Vec::with_capacity(rows.len());
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
