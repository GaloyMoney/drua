mod entity;
pub mod error;
pub(crate) mod repo;

use tracing::instrument;

pub use entity::Workspace;
use entity::*;
pub use error::*;
use repo::*;

use crate::primitives::*;

#[derive(Clone)]
pub struct Workspaces {
    repo: WorkspaceRepo,
}

impl Workspaces {
    pub fn new(pool: &sqlx::PgPool) -> Self {
        let repo = WorkspaceRepo::new(pool);
        Self { repo }
    }

    #[instrument(name = "domain.workspace.create", skip(self))]
    pub async fn create(
        &self,
        name: impl Into<String> + std::fmt::Debug,
        description: Option<String>,
    ) -> Result<Workspace, WorkspaceError> {
        let new_workspace = build_new_workspace(name, description);
        let workspace = self.repo.create(new_workspace).await?;
        Ok(workspace)
    }

    #[instrument(name = "domain.workspace.find_by_id", skip(self))]
    pub async fn find_by_id(
        &self,
        id: impl Into<WorkspaceId> + std::fmt::Debug,
    ) -> Result<Workspace, WorkspaceError> {
        Ok(self.repo.find_by_id(id.into()).await?)
    }

    #[instrument(name = "domain.workspace.list_all", skip(self))]
    pub async fn list_all(&self) -> Result<Vec<Workspace>, WorkspaceError> {
        Ok(self.repo.list_all().await?)
    }

    #[instrument(name = "domain.workspace.update", skip(self))]
    pub async fn update(
        &self,
        id: impl Into<WorkspaceId> + std::fmt::Debug,
        name: impl Into<String> + std::fmt::Debug,
        description: Option<String>,
    ) -> Result<Workspace, WorkspaceError> {
        let mut workspace = self.repo.find_by_id(id.into()).await?;
        workspace.update(name, description);
        self.repo.update(&mut workspace).await?;
        Ok(workspace)
    }
}

fn build_new_workspace(name: impl Into<String>, description: Option<String>) -> NewWorkspace {
    let mut builder = NewWorkspace::builder();
    builder.name(name);
    if let Some(desc) = description {
        builder.description(desc);
    }
    builder.build().expect("Could not build new workspace")
}
