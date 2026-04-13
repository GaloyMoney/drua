use sqlx::PgPool;

use es_entity::*;
use primitives::AgentId;

use super::{entity::*, thread::*, AgentSessionId};

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "AgentSession",
    columns(agent_id(ty = "AgentId", list_for(by(created_at))))
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
}

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "SessionThread",
    columns(session_id(ty = "AgentSessionId", update(persist = false), parent))
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
