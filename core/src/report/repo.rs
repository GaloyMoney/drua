use sqlx::PgPool;

use es_entity::*;

use crate::primitives::*;

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(entity = "Report", columns(title(ty = "String")))]
pub struct ReportRepo {
    #[allow(dead_code)]
    pool: PgPool,
}

impl ReportRepo {
    pub fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }
}
