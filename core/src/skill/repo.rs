use sqlx::PgPool;

use es_entity::*;

use crate::primitives::*;

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "Skill",
    columns(
        project_id(
            ty = "Option<ProjectId>",
            list_for(by(created_at)),
            constraint = "idx_skills_project_name",
        ),
        space_id(
            ty = "Option<SpaceId>",
            list_for(by(created_at)),
            constraint = "idx_skills_space_name",
        ),
        name(
            ty = "String",
            list_for(by(created_at)),
            constraint = "idx_skills_global_name",
        ),
        path(ty = "String", update(persist = "false"),),
    ),
    delete = "soft_without_queries",
    post_persist_hook(method = "sync_to_library", error = "drua_library::LibraryError")
)]
pub struct SkillRepo {
    #[allow(dead_code)]
    pool: PgPool,
    library: Option<drua_library::Library>,
}

impl SkillRepo {
    pub fn new(pool: &PgPool, library: drua_library::Library) -> Self {
        Self {
            pool: pool.clone(),
            library: Some(library),
        }
    }

    /// Skips when no library is configured (e.g. in tests).
    async fn sync_to_library<OP: es_entity::AtomicOperation>(
        &self,
        op: &mut OP,
        entity: &Skill,
        mut new_events: es_entity::LastPersisted<'_, SkillEvent>,
    ) -> Result<(), drua_library::LibraryError> {
        if let Some(library) = &self.library {
            library.sync_entity_in_op(op, entity, &mut new_events).await
        } else {
            Ok(())
        }
    }
}

impl SkillRepo {
    pub fn new_without_library(pool: &PgPool) -> Self {
        Self {
            pool: pool.clone(),
            library: None,
        }
    }

    /// Bulk soft-delete all project-scoped skills. No event is generated
    /// because the repo uses `soft_without_queries`, making a column update
    /// equivalent to iterating each entity through `delete_in_op`.
    pub async fn cascade_delete_for_project_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        project_id: ProjectId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE skills SET deleted = TRUE WHERE project_id = $1")
            .bind(project_id)
            .execute(op.as_executor())
            .await?;
        Ok(())
    }
}
