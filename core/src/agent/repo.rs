use sqlx::PgPool;

use crate::primitives::{AgentId, WorkflowDefinitionId, WorkflowRunId, WorkspaceId};
use es_entity::*;

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "Agent",
    columns(
        workspace_id(ty = "WorkspaceId", list_for(by(created_at))),
        workflow_id(ty = "Option<WorkflowDefinitionId>"),
        workflow_run_id(ty = "Option<WorkflowRunId>", list_for(by(created_at))),
    ),
    delete = "soft_without_queries"
)]
pub struct AgentRepo {
    #[allow(dead_code)]
    pool: PgPool,
}

impl AgentRepo {
    pub fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }
}
