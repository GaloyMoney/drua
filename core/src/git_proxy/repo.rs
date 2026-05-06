use sqlx::PgPool;

use es_entity::*;

use crate::primitives::*;

use super::entity::*;
use super::primitives::GitProxyAllowlistEntryId;

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "GitProxyAllowlistEntry",
    columns(
        project_id(ty = "ProjectId", list_for(by(created_at))),
        owner(ty = "String"),
        repo(ty = "String"),
    ),
    delete = "soft_without_queries"
)]
pub struct GitProxyAllowlistEntryRepo {
    #[allow(dead_code)]
    pool: PgPool,
}

impl GitProxyAllowlistEntryRepo {
    pub fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }
}
