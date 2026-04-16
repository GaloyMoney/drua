mod entity;
pub mod error;
pub(crate) mod repo;

use std::sync::Arc;

use tracing::instrument;

pub use crate::primitives::*;
use crate::sandbox::Sandboxes;
pub use entity::*;
pub use error::*;
use repo::*;

#[derive(Clone)]
pub struct Skills {
    repo: SkillRepo,
    sandboxes: Arc<Sandboxes>,
}

impl Skills {
    pub fn new(pool: &sqlx::PgPool, sandboxes: Arc<Sandboxes>) -> Self {
        let repo = SkillRepo::new(pool);
        Self { repo, sandboxes }
    }

    pub fn sandboxes(&self) -> &Sandboxes {
        &self.sandboxes
    }

    #[instrument(name = "skill.create", skip_all)]
    pub async fn create(&self, new: NewSkill) -> Result<Skill, SkillError> {
        let skill = self.repo.create(new).await?;
        Ok(skill)
    }

    #[instrument(name = "skill.create_in_op", skip_all)]
    pub async fn create_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        new: NewSkill,
    ) -> Result<Skill, SkillError> {
        let skill = self.repo.create_in_op(op, new).await?;
        Ok(skill)
    }

    #[instrument(name = "skill.find_by_id", skip_all)]
    pub async fn find_by_id(&self, id: SkillId) -> Result<Skill, SkillError> {
        Ok(self.repo.find_by_id(id).await?)
    }

    /// Resolve a skill by name and return its body, falling back to the
    /// optionally-supplied sandbox's exported skills when there's no
    /// match in the repo.
    ///
    /// Lookup order:
    /// 1. `repo.maybe_find_by_name(name)` — workspace-scoped registered skills
    ///    (see the `idx_skills_workspace_name` unique index in the migration).
    /// 2. If `sandbox_id` is `Some`, load the sandbox and scan its
    ///    `exported_skills` (populated by `/initialize` from the cloned
    ///    repo's `.claude/commands/*.md`) for a matching `name`.
    ///
    /// Returns `Ok(None)` when neither source matches; returns errors
    /// only on actual repo / sandbox-lookup failures.
    #[instrument(name = "skill.find_by_name", skip_all, fields(name = %name, sandbox_id))]
    pub async fn find_by_name(
        &self,
        name: &str,
        sandbox_id: Option<SandboxId>,
    ) -> Result<Option<String>, SkillError> {
        if let Some(skill) = self.repo.maybe_find_by_name(name.to_string()).await? {
            return Ok(Some(skill.body));
        }

        if let Some(sandbox_id) = sandbox_id {
            tracing::Span::current().record("sandbox_id", sandbox_id.to_string());
            // `maybe_find_by_id` returns `None` for a missing sandbox —
            // skill lookup treats that as "no fallback available" rather
            // than an error.
            let sandbox = self
                .sandboxes
                .maybe_find_by_id(sandbox_id)
                .await
                .map_err(|e| SkillError::SandboxLookup(e.to_string()))?;
            if let Some(body) = sandbox.and_then(|s| s.find_skill(name)) {
                return Ok(Some(body));
            }
        }

        Ok(None)
    }

    #[instrument(name = "skill.list_by_workspace_id", skip_all)]
    pub async fn list_by_workspace_id(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<Skill>, SkillError> {
        let query = es_entity::PaginatedQueryArgs {
            first: 100,
            after: None,
        };
        let result = self
            .repo
            .list_for_workspace_id_by_created_at(
                workspace_id,
                query,
                es_entity::ListDirection::Descending,
            )
            .await?;
        Ok(result.entities)
    }

    #[instrument(name = "skill.update", skip_all)]
    pub async fn update(&self, skill: &mut Skill) -> Result<(), SkillError> {
        self.repo.update(skill).await?;
        Ok(())
    }

    #[instrument(name = "skill.delete", skip_all)]
    pub async fn delete(&self, id: SkillId) -> Result<(), SkillError> {
        let skill = self.repo.find_by_id(id).await?;
        self.repo.delete(skill).await?;
        Ok(())
    }

    #[instrument(name = "skill.delete_in_op", skip_all)]
    pub async fn delete_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        id: SkillId,
    ) -> Result<(), SkillError> {
        let skill = self.repo.find_by_id(id).await?;
        self.repo.delete_in_op(op, skill).await?;
        Ok(())
    }
}
