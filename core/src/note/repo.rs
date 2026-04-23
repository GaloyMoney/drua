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

    /// Post-persist hook: sync note content to the git-backed library.
    /// Only fires on content changes (Initialized/Updated); skips pin/unpin.
    async fn sync_to_library<OP: es_entity::AtomicOperation>(
        &self,
        op: &mut OP,
        entity: &Note,
        mut new_events: es_entity::LastPersisted<'_, NoteEvent>,
    ) -> Result<(), crate::library::LibraryError> {
        let needs_sync = new_events.any(|persisted| {
            matches!(
                &persisted.event,
                NoteEvent::Initialized { .. } | NoteEvent::Updated { .. }
            )
        });
        if needs_sync {
            let runtime_file = entity.as_runtime_file();
            self.library.write_in_op(op, &runtime_file).await?;
        }
        Ok(())
    }
}
