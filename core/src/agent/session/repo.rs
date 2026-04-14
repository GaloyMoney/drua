use sqlx::PgPool;

use crate::primitives::AgentId;
use es_entity::*;

use super::{entity::*, AgentSessionId};

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "AgentSession",
    columns(agent_id(ty = "AgentId")),
    delete = "soft_without_queries"
)]
pub struct AgentSessionRepo {
    #[allow(dead_code)]
    pool: PgPool,
}

impl AgentSessionRepo {
    pub fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }

    /// Soft-delete the session belonging to `agent_id`. No-op when the agent
    /// has no session.
    pub async fn cascade_delete_for_agent_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        agent_id: AgentId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE agent_sessions SET deleted = TRUE WHERE agent_id = $1")
            .bind(agent_id)
            .execute(op.as_executor())
            .await?;
        Ok(())
    }
}
