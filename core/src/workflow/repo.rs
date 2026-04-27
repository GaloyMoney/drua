use sqlx::PgPool;

use es_entity::*;

use crate::library::Library;
use crate::primitives::*;

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "WorkflowDefinition",
    columns(
        workspace_id(ty = "WorkspaceId", list_for(by(created_at))),
        name(ty = "String"),
    ),
    post_persist_hook(method = "sync_to_library", error = "crate::library::LibraryError")
)]
pub struct WorkflowDefinitionRepo {
    #[allow(dead_code)]
    pool: PgPool,
    library: Option<Library>,
}

impl WorkflowDefinitionRepo {
    pub fn new(pool: &PgPool, library: Library) -> Self {
        Self {
            pool: pool.clone(),
            library: Some(library),
        }
    }

    pub fn new_without_library(pool: &PgPool) -> Self {
        Self {
            pool: pool.clone(),
            library: None,
        }
    }

    /// Post-persist hook: forward-sync the workflow definition to the
    /// git-backed library. Mirrors `SkillRepo::sync_to_library` — only
    /// fires for `Initialized` / `Updated` events (no library traffic
    /// on no-op writes), and skips entirely when no library is
    /// configured (e.g. in tests).
    async fn sync_to_library<OP: es_entity::AtomicOperation>(
        &self,
        op: &mut OP,
        entity: &WorkflowDefinition,
        mut new_events: es_entity::LastPersisted<'_, WorkflowDefinitionEvent>,
    ) -> Result<(), crate::library::LibraryError> {
        let library = match &self.library {
            Some(lib) => lib,
            None => return Ok(()),
        };
        let needs_sync = new_events.any(|persisted| {
            matches!(
                &persisted.event,
                WorkflowDefinitionEvent::Initialized { .. }
                    | WorkflowDefinitionEvent::Updated { .. }
            )
        });
        if needs_sync {
            let runtime_file = entity.as_runtime_file();
            library.write_in_op(op, &runtime_file).await?;
        }
        Ok(())
    }
}
