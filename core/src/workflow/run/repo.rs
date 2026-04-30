use sqlx::PgPool;

use es_entity::*;

use crate::primitives::*;

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "WorkflowRun",
    columns(
        project_id(ty = "ProjectId", list_for(by(created_at))),
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

    pub async fn cascade_delete_for_project_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        project_id: ProjectId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE workflow_runs SET deleted = TRUE WHERE project_id = $1")
            .bind(project_id)
            .execute(op.as_executor())
            .await?;
        Ok(())
    }
}
