use sqlx::PgPool;

use crate::primitives::AgentId;
use es_entity::*;

use super::{entity::*, thread::*, AgentSessionId};

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "AgentSession",
    columns(
        agent_id(ty = "AgentId"),
        is_chain_inherited(
            ty = "bool",
            create(accessor = "is_chain_inherited()"),
            update(accessor = "is_chain_inherited()"),
            list_for(by(created_at)),
        ),
    ),
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
