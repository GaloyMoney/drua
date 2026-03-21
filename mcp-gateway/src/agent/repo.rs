use sqlx::PgPool;

use es_entity::*;

use crate::primitives::*;

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "Agent",
    columns(user_id(ty = "UserId", list_for(by(created_at))))
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
