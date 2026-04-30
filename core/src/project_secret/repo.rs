use sqlx::PgPool;

use es_entity::*;

use crate::primitives::*;

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "ProjectSecret",
    columns(
        project_id(ty = "ProjectId", list_for(by(created_at))),
        name(ty = "String")
    ),
    delete = "soft_without_queries"
)]
pub struct ProjectSecretRepo {
    #[allow(dead_code)]
    pool: PgPool,
}

impl ProjectSecretRepo {
    pub fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }

    /// Bulk soft-delete all secrets belonging to a project. No event is
    /// generated because the repo uses `soft_without_queries`, making a
    /// column update equivalent to iterating each entity through
    /// `delete_in_op`.
    pub async fn cascade_delete_for_project_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        project_id: ProjectId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE project_secrets SET deleted = TRUE WHERE project_id = $1")
            .bind(project_id)
            .execute(op.as_executor())
            .await?;
        Ok(())
    }
}
