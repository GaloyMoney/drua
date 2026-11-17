use sqlx::PgPool;

use es_entity::*;

use crate::primitives::*;

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "Sandbox",
    columns(
        project_id(ty = "ProjectId", list_for(by(created_at))),
        name(ty = "String", list_for(by(created_at))),
        workflow_id(ty = "Option<WorkflowDefinitionId>", list_for(by(created_at))),
    ),
    delete = "soft_without_queries"
)]
pub struct SandboxRepo {
    #[allow(dead_code)]
    pool: PgPool,
}

impl SandboxRepo {
    pub fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }

    /// Bulk soft-delete every sandbox row belonging to `project_id`.
    /// `soft_without_queries` makes a column update equivalent to
    /// iterating each entity through `delete_in_op`; no events generated.
    /// Pod / PVC teardown happens out-of-band via `AdminClient` before
    /// callers invoke this — the row flip is the final tombstone.
    pub async fn cascade_delete_for_project_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        project_id: ProjectId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE sandboxes SET deleted = TRUE WHERE project_id = $1")
            .bind(project_id)
            .execute(op.as_executor())
            .await?;
        Ok(())
    }

    pub async fn cascade_delete_for_workflow_id_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        workflow_id: WorkflowDefinitionId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE sandboxes SET deleted = TRUE WHERE workflow_id = $1")
            .bind(workflow_id)
            .execute(op.as_executor())
            .await?;
        Ok(())
    }

    /// Resource names (`sb-<id>`) of every non-deleted sandbox. The PVC
    /// janitor diffs the k8s PVC `sandbox-name` labels against this set;
    /// any PVC owner absent here is a leaked volume. Expression must
    /// match `Sandbox::resource_name`.
    pub async fn live_resource_names(&self) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar("SELECT 'sb-' || id::text FROM sandboxes WHERE NOT deleted")
            .fetch_all(&self.pool)
            .await
    }
}
