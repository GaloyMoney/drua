use sqlx::PgPool;

use crate::primitives::{AgentId, ProjectId, WorkflowDefinitionId, WorkflowRunId};
use es_entity::*;

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "Agent",
    columns(
        project_id(ty = "ProjectId", list_for(by(created_at))),
        workflow_id(ty = "Option<WorkflowDefinitionId>", list_for(by(created_at))),
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
