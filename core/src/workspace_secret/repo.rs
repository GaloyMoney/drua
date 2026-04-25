use sqlx::PgPool;

use es_entity::*;

use crate::primitives::*;

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "WorkspaceSecret",
    columns(
        workspace_id(ty = "WorkspaceId", list_for(by(created_at))),
        name(ty = "String")
    ),
    delete = "soft_without_queries"
)]
pub struct WorkspaceSecretRepo {
    #[allow(dead_code)]
    pool: PgPool,
}

impl WorkspaceSecretRepo {
    pub fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }

    /// Bulk soft-delete all secrets belonging to a workspace. No event is
    /// generated because the repo uses `soft_without_queries`, making a
    /// column update equivalent to iterating each entity through
    /// `delete_in_op`.
    pub async fn cascade_delete_for_workspace_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        workspace_id: WorkspaceId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE workspace_secrets SET deleted = TRUE WHERE workspace_id = $1")
            .bind(workspace_id)
            .execute(op.as_executor())
            .await?;
        Ok(())
    }
}
