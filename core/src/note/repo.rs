use sqlx::PgPool;

use es_entity::*;

use crate::library::Library;
use crate::primitives::*;

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "Note",
    columns(
        workspace_id(ty = "WorkspaceId", list_for(by(created_at))),
        pinned(ty = "bool"),
        workflow_id(ty = "Option<WorkflowDefinitionId>", list_for(by(created_at))),
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
    pub async fn cascade_delete_for_workspace_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        workspace_id: WorkspaceId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE notes SET deleted = TRUE WHERE workspace_id = $1")
            .bind(workspace_id)
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
        crate::library::sync_to_library(Some(&self.library), op, entity, &mut new_events).await
    }
}
