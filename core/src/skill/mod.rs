mod entity;
pub mod error;
pub(crate) mod repo;

use tracing::instrument;

pub use crate::primitives::*;
use crate::sandbox::Sandboxes;
pub use entity::*;
pub use error::*;
use repo::*;

#[derive(Clone)]
pub struct Skills {
    repo: SkillRepo,
    sandboxes: Sandboxes,
}

impl Skills {
    pub fn new(pool: &sqlx::PgPool, sandboxes: Sandboxes) -> Self {
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
