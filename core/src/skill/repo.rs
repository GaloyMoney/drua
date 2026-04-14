use sqlx::PgPool;

use es_entity::*;

use crate::primitives::*;

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "Skill",
    columns(
        workspace_id(ty = "WorkspaceId", list_for(by(created_at))),
        name(ty = "String")
    ),
    delete = "soft_without_queries"
)]
pub struct SkillRepo {
    #[allow(dead_code)]
    pool: PgPool,
}

impl SkillRepo {
    pub fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }
}
