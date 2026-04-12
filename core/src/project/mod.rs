mod entity;
pub mod error;
pub(crate) mod repo;

use tracing::instrument;

pub use entity::Project;
use entity::*;
pub use error::*;
use repo::*;

use crate::primitives::*;

#[derive(Clone)]
pub struct Projects {
    repo: ProjectRepo,
}

impl Projects {
    pub fn new(pool: &sqlx::PgPool) -> Self {
        let repo = ProjectRepo::new(pool);
        Self { repo }
    }

    #[instrument(name = "domain.project.create", skip(self))]
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        name: impl Into<String> + std::fmt::Debug,
        description: Option<String>,
    ) -> Result<Project, ProjectError> {
        let mut op = self.repo.begin_op().await?;
        let project = self
            .create_in_op(&mut op, workspace_id, name, description)
            .await?;
        op.commit().await?;
        Ok(project)
    }

    #[instrument(name = "domain.project.create_in_op", skip(self, op))]
    pub async fn create_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        workspace_id: WorkspaceId,
        name: impl Into<String> + std::fmt::Debug,
        description: Option<String>,
    ) -> Result<Project, ProjectError> {
        let name = name.into();

        // Check name uniqueness within workspace
        let existing = self.list_for_workspace(workspace_id).await?;
        if existing.iter().any(|p| p.name == name) {
            return Err(ProjectError::NameAlreadyExists(name));
        }

        let new_project = build_new_project(workspace_id, &name, description);
        let project = self.repo.create_in_op(op, new_project).await?;
        Ok(project)
    }

    #[instrument(name = "domain.project.find_by_id", skip(self))]
    pub async fn find_by_id(
        &self,
        id: impl Into<ProjectId> + std::fmt::Debug,
    ) -> Result<Project, ProjectError> {
        Ok(self.repo.find_by_id(id.into()).await?)
    }

    #[instrument(name = "domain.project.list_for_workspace", skip(self))]
    pub async fn list_for_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<Project>, ProjectError> {
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
        Ok(result
            .entities
            .into_iter()
            .filter(|p| !p.is_archived())
            .collect())
    }

    #[instrument(name = "domain.project.update", skip(self))]
    pub async fn update(
        &self,
        id: impl Into<ProjectId> + std::fmt::Debug,
        name: impl Into<String> + std::fmt::Debug,
        description: Option<String>,
    ) -> Result<Project, ProjectError> {
        let id = id.into();
        let name = name.into();

        // Check name uniqueness (excluding self)
        let project = self.repo.find_by_id(id).await?;
        let existing = self.list_for_workspace(project.workspace_id).await?;
        if existing.iter().any(|p| p.name == name && p.id != id) {
            return Err(ProjectError::NameAlreadyExists(name));
        }

        let mut project = project;
        project.update(name, description);
        self.repo.update(&mut project).await?;
        Ok(project)
    }

    #[instrument(name = "domain.project.archive", skip(self))]
    pub async fn archive(
        &self,
        id: impl Into<ProjectId> + std::fmt::Debug,
    ) -> Result<Project, ProjectError> {
        let mut project = self.repo.find_by_id(id.into()).await?;
        if project.change_status(ProjectStatus::Archived).did_execute() {
            self.repo.update(&mut project).await?;
        }
        Ok(project)
    }

    #[instrument(name = "domain.project.archive_in_op", skip(self, op))]
    pub async fn archive_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        id: impl Into<ProjectId> + std::fmt::Debug,
    ) -> Result<Project, ProjectError> {
        let mut project = self.repo.find_by_id(id.into()).await?;
        if project.change_status(ProjectStatus::Archived).did_execute() {
            self.repo.update_in_op(op, &mut project).await?;
        }
        Ok(project)
    }

    #[instrument(name = "domain.project.change_status", skip(self))]
    pub async fn change_status(
        &self,
        id: impl Into<ProjectId> + std::fmt::Debug,
        status: ProjectStatus,
    ) -> Result<Project, ProjectError> {
        let mut project = self.repo.find_by_id(id.into()).await?;
        if project.change_status(status).did_execute() {
            self.repo.update(&mut project).await?;
        }
        Ok(project)
    }
}

fn build_new_project(
    workspace_id: WorkspaceId,
    name: impl Into<String>,
    description: Option<String>,
) -> NewProject {
    let mut builder = NewProject::builder();
    builder.workspace_id(workspace_id);
    builder.name(name);
    if let Some(desc) = description {
        builder.description(desc);
    }
    builder.build().expect("Could not build new project")
}
