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

    /// Returns all runs currently parked on a wait step. Uses the
    /// events table to find runs that have a `step_waiting` event
    /// without a subsequent `step_resumed` or `run_completed` event.
    pub async fn list_waiting_for_event(&self) -> Result<Vec<WorkflowRun>, sqlx::Error> {
        let ids: Vec<(uuid::Uuid,)> = sqlx::query_as(
            r#"
            SELECT DISTINCT e.id
            FROM workflow_run_events e
            JOIN workflow_runs r ON r.id = e.id AND r.deleted = FALSE
            WHERE e.event_type = 'step_waiting'
              AND NOT EXISTS (
                SELECT 1 FROM workflow_run_events e2
                WHERE e2.id = e.id
                  AND e2.event_type IN ('step_resumed', 'run_completed')
                  AND e2.sequence > e.sequence
              )
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        let mut runs = Vec::with_capacity(ids.len());
        for (id,) in ids {
            let run_id = WorkflowRunId::from(id);
            if let Ok(run) = self.find_by_id(run_id).await {
                runs.push(run);
            }
        }
        Ok(runs)
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

    pub async fn cascade_delete_for_definition_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        definition_id: WorkflowDefinitionId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE workflow_runs SET deleted = TRUE WHERE definition_id = $1")
            .bind(definition_id)
            .execute(op.as_executor())
            .await?;
        Ok(())
    }
}
