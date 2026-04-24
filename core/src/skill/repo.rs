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

    /// List all global skills (workspace_id IS NULL).
    ///
    /// Uses a raw query because es-entity 0.10.34's generated
    /// `list_for_workspace_id_by_created_at(None)` matches ALL rows when the
    /// parameter is NULL. Fixed in es-entity 0.10.35 but blocked on the `job`
    /// crate being rebuilt against 0.10.35.
    pub async fn list_global(&self) -> Result<Vec<Skill>, SkillFindError> {
        let (skills, _) = es_query!(
            "SELECT id, created_at FROM skills WHERE workspace_id IS NULL AND deleted = FALSE ORDER BY created_at ASC LIMIT $1",
            100i64
        )
        .fetch_n(&self.pool, 100)
        .await?;
        Ok(skills)
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
}
