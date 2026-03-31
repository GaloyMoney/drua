use sqlx::PgPool;

use es_entity::*;

use crate::primitives::*;

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(entity = "Memory", columns(title(ty = "String")))]
pub struct MemoryRepo {
    #[allow(dead_code)]
    pool: PgPool,
}

impl MemoryRepo {
    pub fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }
}
