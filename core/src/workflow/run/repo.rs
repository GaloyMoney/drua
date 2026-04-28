use sqlx::PgPool;

use es_entity::*;

use crate::primitives::*;

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "WorkflowRun",
    columns(
        workspace_id(ty = "WorkspaceId", list_for(by(created_at))),
        definition_id(ty = "WorkflowDefinitionId", list_for(by(created_at))),
    ),
    delete = "soft_without_queries"
)]
pub struct WorkflowRunRepo {
    #[allow(dead_code)]
    pool: PgPool,
}

impl WorkflowRunRepo {
    pub fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }

    pub async fn cascade_delete_for_workspace_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        workspace_id: WorkspaceId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE workflow_runs SET deleted = TRUE WHERE workspace_id = $1")
            .bind(workspace_id)
            .execute(op.as_executor())
            .await?;
        Ok(())
    }
}
