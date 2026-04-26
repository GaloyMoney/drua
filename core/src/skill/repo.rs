use sqlx::PgPool;

use es_entity::*;

use crate::library::Library;
use crate::primitives::*;

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "Skill",
    columns(
        workspace_id(ty = "Option<WorkspaceId>", list_for(by(created_at))),
        name(ty = "String", list_for(by(created_at)))
    ),
    delete = "soft_without_queries",
    post_persist_hook(method = "sync_to_library", error = "crate::library::LibraryError")
)]
pub struct SkillRepo {
    #[allow(dead_code)]
    pool: PgPool,
    library: Option<Library>,
}

impl SkillRepo {
    pub fn new(pool: &PgPool, library: Library) -> Self {
        Self {
            pool: pool.clone(),
            library: Some(library),
        }
    }

    /// Post-persist hook: sync skill content to the git-backed library.
    /// Only fires on content changes (Initialized/Updated).
    /// Skips when no library is configured (e.g. in tests).
    async fn sync_to_library<OP: es_entity::AtomicOperation>(
        &self,
        op: &mut OP,
        entity: &Skill,
        mut new_events: es_entity::LastPersisted<'_, SkillEvent>,
    ) -> Result<(), crate::library::LibraryError> {
        let library = match &self.library {
            Some(lib) => lib,
            None => return Ok(()),
        };
        let needs_sync = new_events.any(|persisted| {
            matches!(
                &persisted.event,
                SkillEvent::Initialized { .. } | SkillEvent::Updated { .. }
            )
        });
        if needs_sync {
            let runtime_file = entity.as_runtime_file();
            library.write_in_op(op, &runtime_file).await?;
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

    /// Bulk soft-delete all workspace-scoped skills. No event is generated
    /// because the repo uses `soft_without_queries`, making a column update
    /// equivalent to iterating each entity through `delete_in_op`.
    pub async fn cascade_delete_for_workspace_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        workspace_id: WorkspaceId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE skills SET deleted = TRUE WHERE workspace_id = $1")
            .bind(workspace_id)
            .execute(op.as_executor())
            .await?;
        Ok(())
    }
}
