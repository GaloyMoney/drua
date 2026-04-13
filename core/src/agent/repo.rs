use sqlx::PgPool;

use es_entity::*;
use crate::primitives::{AgentId, WorkspaceId};

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "Agent",
    columns(workspace_id(ty = "WorkspaceId", list_for(by(created_at)))),
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
