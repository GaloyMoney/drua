use sqlx::PgPool;

use es_entity::*;

use crate::primitives::*;

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "Note",
    columns(
        project_id(ty = "Option<ProjectId>", list_for(by(created_at))),
        space_id(ty = "Option<SpaceId>", list_for(by(created_at))),
        path(ty = "String"),
    ),
    delete = "soft_without_queries",
    post_persist_hook(method = "sync_to_library", error = "drua_library::LibraryError")
)]
pub struct NoteRepo {
    #[allow(dead_code)]
    pool: PgPool,
    library: drua_library::Library,
}

impl NoteRepo {
    pub fn new(pool: &PgPool, library: drua_library::Library) -> Self {
        Self {
            pool: pool.clone(),
            library,
        }
    }

    /// `soft_without_queries` makes this column update equivalent to
    /// iterating each entity through `delete_in_op`; no events generated.
    pub async fn cascade_delete_for_project_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        project_id: ProjectId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE notes SET deleted = TRUE WHERE project_id = $1")
            .bind(project_id)
            .execute(op.as_executor())
            .await?;
        Ok(())
    }

    /// Flip `deleted = FALSE` on a soft-deleted note row with this id.
    /// Mirrors `SkillRepo::maybe_revive_in_op` — used by reverse-sync
    /// when a `spaces edit op=move` re-imports the previously-deleted
    /// frontmatter id at a new path.
    pub async fn maybe_revive_in_op(
        &self,
        op: &mut impl AtomicOperation,
        id: NoteId,
    ) -> Result<bool, sqlx::Error> {
        let row_id: uuid::Uuid = id.into();
        let result = sqlx::query!(
            "UPDATE notes SET deleted = FALSE WHERE id = $1 AND deleted = TRUE",
            row_id
        )
        .execute(op.as_executor())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Fires only on content events (Initialized/Updated); skips pin/unpin.
    async fn sync_to_library<OP: es_entity::AtomicOperation>(
        &self,
        op: &mut OP,
        entity: &Note,
        mut new_events: es_entity::LastPersisted<'_, NoteEvent>,
    ) -> Result<(), drua_library::LibraryError> {
        self.library
            .sync_entity_in_op(op, entity, &mut new_events)
            .await
    }
}
