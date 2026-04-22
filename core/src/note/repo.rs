use sqlx::PgPool;

use es_entity::*;

use crate::primitives::*;

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "Note",
    columns(workspace_id(ty = "WorkspaceId", list_for(by(created_at)))),
    delete = "soft_without_queries"
)]
pub struct NoteRepo {
    #[allow(dead_code)]
    pool: PgPool,
}

impl NoteRepo {
    pub fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }
}
