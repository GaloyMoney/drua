use sqlx::PgPool;

use es_entity::*;
use crate::primitives::AgentId;

use super::{entity::*, thread::*, AgentSessionId};

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "AgentSession",
    columns(agent_id(ty = "AgentId")),
    delete = "soft_without_queries"
)]
pub struct AgentSessionRepo {
    #[allow(dead_code)]
    pool: PgPool,

    #[es_repo(nested)]
    threads: SessionThreadRepo,
}

impl AgentSessionRepo {
    pub fn new(pool: &PgPool) -> Self {
        Self {
            pool: pool.clone(),
            threads: SessionThreadRepo::new(pool),
        }
    }

    /// Soft-delete the session belonging to `agent_id` and every thread under
    /// it. No-op when the agent has no session. Soft delete on this entity
    /// family is a column update with no event, so a bulk SQL update is
    /// equivalent to iterating each entity through `delete_in_op`.
    pub async fn cascade_delete_for_agent_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        agent_id: AgentId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE session_threads SET deleted = TRUE \
             WHERE session_id IN (SELECT id FROM agent_sessions WHERE agent_id = $1)",
        )
        .bind(agent_id)
        .execute(op.as_executor())
        .await?;
        sqlx::query("UPDATE agent_sessions SET deleted = TRUE WHERE agent_id = $1")
            .bind(agent_id)
            .execute(op.as_executor())
            .await?;
        Ok(())
    }
}

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "SessionThread",
    columns(session_id(ty = "AgentSessionId", update(persist = false), parent)),
    delete = "soft_without_queries"
)]
struct SessionThreadRepo {
    #[allow(dead_code)]
    pool: PgPool,
}

impl SessionThreadRepo {
    fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }
}
