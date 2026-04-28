use sqlx::PgPool;

use es_entity::*;

use crate::primitives::*;

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "Sandbox",
    columns(
        workspace_id(ty = "WorkspaceId", list_for(by(created_at))),
        name(ty = "String"),
        workflow_id(ty = "Option<WorkflowDefinitionId>"),
    )
)]
pub struct SandboxRepo {
    #[allow(dead_code)]
    pool: PgPool,
}

impl SandboxRepo {
    pub fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }
}
