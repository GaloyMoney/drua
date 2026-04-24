use sqlx::PgPool;

use es_entity::*;

use crate::library::Library;
use crate::primitives::*;

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "Skill",
    columns(workspace_id(ty = "Option<WorkspaceId>", list_for(by(created_at)))),
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

    /// Find a workspace-scoped skill by name.
    pub async fn find_by_name_in_workspace(
        &self,
        workspace_id: WorkspaceId,
        name: &str,
    ) -> Result<Option<Skill>, SkillFindError> {
        let ws_id = uuid::Uuid::from(workspace_id);
        let row: Option<(uuid::Uuid,)> = sqlx::query_as(
            "SELECT id FROM skills WHERE workspace_id = $1 AND name = $2 AND deleted = FALSE",
        )
        .bind(ws_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(SkillFindError::from)?;

        match row {
            Some((id,)) => Ok(Some(self.find_by_id(SkillId::from(id)).await?)),
            None => Ok(None),
        }
    }

    /// Find a global (unscoped) skill by name.
    pub async fn find_by_name_global(&self, name: &str) -> Result<Option<Skill>, SkillFindError> {
        let row: Option<(uuid::Uuid,)> = sqlx::query_as(
            "SELECT id FROM skills WHERE workspace_id IS NULL AND name = $1 AND deleted = FALSE",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(SkillFindError::from)?;

        match row {
            Some((id,)) => Ok(Some(self.find_by_id(SkillId::from(id)).await?)),
            None => Ok(None),
        }
    }

    /// List all global skills (no workspace).
    pub async fn list_global(&self) -> Result<Vec<Skill>, SkillFindError> {
        let rows: Vec<(uuid::Uuid,)> = sqlx::query_as(
            "SELECT id FROM skills WHERE workspace_id IS NULL AND deleted = FALSE ORDER BY created_at ASC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(SkillFindError::from)?;

        let mut skills = Vec::with_capacity(rows.len());
        for (id,) in rows {
            skills.push(self.find_by_id(SkillId::from(id)).await?);
        }
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
