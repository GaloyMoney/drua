use sqlx::PgPool;

use es_entity::*;
use primitives::AgentId;

use super::{entity::*, AgentSessionId};

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "AgentSession",
    columns(agent_id(ty = "AgentId", list_for(by(created_at))))
)]
pub struct AgentSessionRepo {
    #[allow(dead_code)]
    pool: PgPool,
}

impl AgentSessionRepo {
    pub fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }
}
