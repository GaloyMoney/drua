use sqlx::PgPool;

use es_entity::*;

use crate::primitives::*;

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "Space",
    columns(slug(ty = "String", list_by)),
    delete = "soft_without_queries"
)]
pub struct SpaceRepo {
    #[allow(dead_code)]
    pool: PgPool,
}

impl SpaceRepo {
    pub fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }
}
