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
        path(ty = "String"),
    ),
    delete = "soft",
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

    /// Content events (Initialized / Updated) go through
    /// `sync_entity_in_op` which fires the full hook (search-row
    /// upsert + git write-back). A pure `PathChanged` event (file
    /// moved with no content delta) only needs the search-store row
    /// refreshed — git already has the file at the new path. Skips
    /// entirely when no library is wired (test contexts).
    async fn sync_to_library<OP: es_entity::AtomicOperation>(
        &self,
        op: &mut OP,
        entity: &Skill,
        mut new_events: es_entity::LastPersisted<'_, SkillEvent>,
    ) -> Result<(), drua_library::LibraryError> {
        let Some(library) = &self.library else {
            return Ok(());
        };
        let has_content = new_events
            .clone()
            .any(|p| <Skill as drua_library::LibrarySynced>::is_content_event(&p.event));
        if has_content {
            return library.sync_entity_in_op(op, entity, &mut new_events).await;
        }
        if new_events.any(|p| matches!(&p.event, SkillEvent::PathChanged { .. })) {
            use drua_library::LibrarySynced;
            library
                .search()
                .upsert_in_op(op, &entity.searchable_fields())
                .await?;
        }
        Ok(())
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

    /// Flip `deleted = FALSE` on a soft-deleted row with this id.
    /// es-entity's `delete = "soft"` auto-generates the
    /// `*_include_deleted` query variants but no native revive — the
    /// flip is a single SQL update. Used by reverse-sync when the
    /// importer encounters a frontmatter id that maps to a previously
    /// soft-deleted row (`spaces edit op=move`).
    pub async fn revive_in_op(
        &self,
        op: &mut impl AtomicOperation,
        id: SkillId,
    ) -> Result<(), sqlx::Error> {
        let row_id: uuid::Uuid = id.into();
        sqlx::query!("UPDATE skills SET deleted = FALSE WHERE id = $1", row_id)
            .execute(op.as_executor())
            .await?;
        Ok(())
    }
}
