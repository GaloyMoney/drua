use sqlx::PgPool;

use es_entity::*;

use crate::primitives::*;

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "Changeset",
    columns(
        project_id(ty = "Option<ProjectId>", list_for(by(created_at))),
        status(
            ty = "ChangesetStatus",
            list_for(by(created_at)),
            create(accessor = "initial_status()"),
            update(accessor = "status")
        ),
        opened_by_actor(
            ty = "String",
            list_for(by(created_at)),
            create(accessor = "opened_by.to_string()"),
            update(persist = false)
        ),
        agent_id(
            ty = "Option<AgentId>",
            list_for(by(created_at)),
            create(accessor = "opened_by.agent_id()"),
            update(persist = false)
        ),
        workflow_run_id(
            ty = "Option<WorkflowRunId>",
            list_for(by(created_at)),
            create(accessor = "opened_by.workflow_run_id()"),
            update(persist = false)
        ),
    ),
    delete = "soft_without_queries"
)]
pub struct ChangesetRepo {
    #[allow(dead_code)]
    pool: PgPool,
}

impl ChangesetRepo {
    pub fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }
}
