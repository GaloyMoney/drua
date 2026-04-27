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
    )
)]
pub struct WorkflowRunRepo {
    #[allow(dead_code)]
    pool: PgPool,
}

impl WorkflowRunRepo {
    pub fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }
}
