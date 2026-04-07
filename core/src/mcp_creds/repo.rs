use sqlx::PgPool;

use es_entity::*;

use crate::primitives::*;

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "McpCreds",
    events_tbl = "mcp_cred_events",
    columns(
        user_id(ty = "UserId", list_for(by(created_at))),
        token_hash(ty = "String", list_by)
    )
)]
pub struct McpCredsRepo {
    #[allow(dead_code)]
    pool: PgPool,
}

impl McpCredsRepo {
    pub fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }
}
