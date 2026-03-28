use sqlx::PgPool;

use es_entity::*;

use crate::primitives::*;

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "Task",
    columns(status(ty = "TaskStatus", list_for, update(accessor = "status")))
)]
pub struct TaskRepo {
    #[allow(dead_code)]
    pool: PgPool,
}

impl TaskRepo {
    pub fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }
}
