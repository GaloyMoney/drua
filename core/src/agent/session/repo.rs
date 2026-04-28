use sqlx::PgPool;

use crate::primitives::AgentId;
use es_entity::*;

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

    /// Soft-delete = no-event column update, so bulk SQL is equivalent to
    /// per-entity `delete_in_op`.
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
