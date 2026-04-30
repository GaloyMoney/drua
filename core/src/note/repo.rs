use sqlx::PgPool;

use es_entity::*;

use crate::library::Library;
use crate::primitives::*;

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "Note",
    columns(
        project_id(ty = "ProjectId", list_for(by(created_at))),
        pinned(ty = "bool"),
    ),
    delete = "soft_without_queries",
    post_persist_hook(method = "sync_to_library", error = "crate::library::LibraryError")
)]
pub struct NoteRepo {
    #[allow(dead_code)]
    pool: PgPool,
    library: Library,
}

impl NoteRepo {
    pub fn new(pool: &PgPool, library: Library) -> Self {
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

    /// Fires only on content events (Initialized/Updated); skips pin/unpin.
    async fn sync_to_library<OP: es_entity::AtomicOperation>(
        &self,
        op: &mut OP,
        entity: &Note,
        mut new_events: es_entity::LastPersisted<'_, NoteEvent>,
    ) -> Result<(), crate::library::LibraryError> {
        self.library
            .sync_entity_in_op(op, entity, &mut new_events)
            .await
    }
}
