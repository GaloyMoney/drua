use sqlx::PgPool;

use es_entity::*;

use crate::primitives::*;

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "WorkspaceSecret",
    columns(
        workspace_id(ty = "WorkspaceId", list_for(by(created_at))),
        name(ty = "String")
    ),
    delete = "soft_without_queries"
)]
pub struct WorkspaceSecretRepo {
    #[allow(dead_code)]
    pool: PgPool,
}

impl WorkspaceSecretRepo {
    pub fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }
}
