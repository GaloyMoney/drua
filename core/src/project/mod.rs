mod entity;
pub mod error;
pub(crate) mod repo;

use std::sync::Arc;

use es_entity::*;
use tracing::instrument;

pub use crate::project::project_cursor::ProjectByCreatedAtCursor;
pub use entity::Project;
use entity::*;
pub use error::*;
use repo::*;

use crate::agent::Agents;
use crate::audit::Audit;
use crate::library::{Library, Space, SpaceError};
use crate::note::Notes;
use crate::primitives::*;
use crate::project_secret::ProjectSecrets;
use crate::sandbox::Sandboxes;
use crate::skill::Skills;
use crate::workflow::Workflows;

#[derive(Clone)]
pub struct Projects {
    repo: ProjectRepo,
    agents: Arc<Agents>,
    sandboxes: Arc<Sandboxes>,
    skills: Arc<Skills>,
    notes: Arc<Notes>,
    secrets: ProjectSecrets,
    workflows: Arc<Workflows>,
    library: Library,
}

impl Projects {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: &sqlx::PgPool,
        agents: Arc<Agents>,
        sandboxes: Arc<Sandboxes>,
        skills: Arc<Skills>,
        notes: Arc<Notes>,
        secrets: ProjectSecrets,
        workflows: Arc<Workflows>,
        library: Library,
    ) -> Self {
        let repo = ProjectRepo::new(pool);
        Self {
            repo,
            agents,
            sandboxes,
            skills,
            notes,
            secrets,
            workflows,
            library,
        }
    }

    #[instrument(name = "domain.project.create", skip(self))]
    pub async fn create(
        &self,
        sub: &AuthSubject,
        name: impl Into<String> + std::fmt::Debug,
        description: Option<String>,
    ) -> Result<Project, ProjectError> {
        sub.can(AuthVerb::Create, AuthResource::Project(None))?;
        Audit::record_action_if_unset("project.create");
        let name = name.into();
        let lead_agent_id = AgentId::new();
        let new_project = build_new_project(lead_agent_id, &name, description);
        let project_id = new_project.id;

        let mut op = self.repo.begin_op().await?;
        let project = self.repo.create_in_op(&mut op, new_project).await?;

        Audit::record_project_id(project_id);
        Audit::record_agent_id(lead_agent_id);

        self.agents
            .create_project_lead_in_op(&mut op, lead_agent_id, project_id, "lead", &name)
            .await?;

        self.library
            .sync_project_folder_in_op(&mut op, &name)
            .await?;

        op.commit().await?;

        tracing::info!(
            project.id = %project.id,
            sub = ?sub,
            "project created"
        );

        Ok(project)
    }

    #[instrument(name = "domain.project.find_by_id", skip(self, sub))]
    pub async fn find_by_id(
        &self,
        sub: &AuthSubject,
        id: impl Into<ProjectId> + std::fmt::Debug,
    ) -> Result<Project, ProjectError> {
        let id = id.into();
        sub.can(AuthVerb::Read, AuthResource::Project(Some(id)))?;
        Audit::record_action_if_unset("project.find_by_id");
        Audit::record_project_id(id);
        Ok(self.repo.find_by_id(id).await?)
    }

    #[instrument(name = "domain.project.list", skip(self, sub))]
    pub async fn list(
        &self,
        sub: &AuthSubject,
        query: PaginatedQueryArgs<ProjectByCreatedAtCursor>,
        direction: ListDirection,
    ) -> Result<PaginatedQueryRet<Project, ProjectByCreatedAtCursor>, ProjectError> {
        sub.can(AuthVerb::Read, AuthResource::Project(None))?;
        Audit::record_action_if_unset("project.list");
        Ok(self.repo.list_by_created_at(query, direction).await?)
    }

    #[instrument(name = "domain.project.list_all", skip(self, sub))]
    pub async fn list_all(&self, sub: &AuthSubject) -> Result<Vec<Project>, ProjectError> {
        sub.can(AuthVerb::Read, AuthResource::Project(None))?;
        Audit::record_action_if_unset("project.list_all");
        Ok(self.repo.list_all().await?)
    }

    #[instrument(name = "domain.project.update", skip(self, sub))]
    pub async fn update(
        &self,
        sub: &AuthSubject,
        id: impl Into<ProjectId> + std::fmt::Debug,
        description: Option<String>,
    ) -> Result<Project, ProjectError> {
        let id = id.into();
        sub.can(AuthVerb::Update, AuthResource::Project(Some(id)))?;
        Audit::record_action_if_unset("project.update");
        Audit::record_project_id(id);
        let mut op = self.repo.begin_op().await?;
        let mut project = self.repo.find_by_id_in_op(&mut op, id).await?;
        if project.update(description).did_execute() {
            self.repo.update_in_op(&mut op, &mut project).await?;
        }
        op.commit().await?;
        Ok(project)
    }

    #[instrument(name = "domain.project.delete", skip(self, sub))]
    pub async fn delete(
        &self,
        sub: &AuthSubject,
        id: impl Into<ProjectId> + std::fmt::Debug,
    ) -> Result<Project, ProjectError> {
        let id = id.into();
        sub.can(AuthVerb::Delete, AuthResource::Project(Some(id)))?;
        Audit::record_action_if_unset("project.delete");
        Audit::record_project_id(id);
        let mut op = self.repo.begin_op().await?;
        let project = self.delete_in_op(&mut op, sub, id).await?;
        op.commit().await?;
        Ok(project)
    }

    #[instrument(name = "domain.project.delete_in_op", skip(self, op, _sub))]
    pub async fn delete_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        _sub: &AuthSubject,
        id: impl Into<ProjectId> + std::fmt::Debug,
    ) -> Result<Project, ProjectError> {
        let id = id.into();
        let mut project = self.repo.find_by_id_in_op(&mut *op, id).await?;

        if !project.archive().did_execute() {
            return Ok(project);
        }

        // Tear down pods/processes via the admin client before touching DB
        // state so in-flight sandbox work stops first.
        if let Err(e) = self.sandboxes.suspend_for_project_in_op(op, id).await {
            tracing::warn!(error = %e, "failed to suspend sandboxes during project delete");
        }

        let agent_list = self.agents.list_for_project_in_op(op, id).await?;
        for agent in &agent_list {
            if let Err(e) = self.agents.delete_in_op(op, agent.id).await {
                tracing::warn!(
                    agent_id = %agent.id,
                    error = %e,
                    "failed to delete agent during project delete"
                );
            }
        }

        if let Err(e) = self.skills.delete_for_project_in_op(op, id).await {
            tracing::warn!(error = %e, "failed to delete skills during project delete");
        }

        if let Err(e) = self.notes.delete_for_project_in_op(op, id).await {
            tracing::warn!(error = %e, "failed to delete notes during project delete");
        }

        if let Err(e) = self.secrets.delete_for_project_in_op(op, id).await {
            tracing::warn!(error = %e, "failed to delete secrets during project delete");
        }

        if let Err(e) = self.workflows.delete_for_project_in_op(op, id).await {
            tracing::warn!(error = %e, "failed to delete workflows during project delete");
        }

        if let Err(e) = self
            .library
            .cleanup_project_in_op(op, uuid::Uuid::from(id), &project.name)
            .await
        {
            tracing::warn!(error = %e, "failed to clean up library during project delete");
        }

        self.repo.update_in_op(&mut *op, &mut project).await?;
        let to_delete = self.repo.find_by_id_in_op(&mut *op, id).await?;
        self.repo.delete_in_op(op, to_delete).await?;
        Ok(project)
    }

    /// Look up a project by name (internal, no auth check).
    /// Returns `Ok(None)` when no project matches.
    #[instrument(name = "domain.project.find_by_name", skip(self))]
    pub(crate) async fn find_by_name(&self, name: &str) -> Result<Option<Project>, ProjectError> {
        Ok(self.repo.maybe_find_by_name(name).await?)
    }

    /// Mounts an existing library space onto `project_id`. Idempotent:
    /// re-mounting a space already in `mounted_spaces` is a no-op. Slug
    /// is resolved via `Library` so dangling ids cannot enter
    /// `mounted_spaces`. Returns the resolved `Space` so callers don't
    /// have to round-trip through `Library` for the entity.
    #[instrument(name = "domain.project.mount_space", skip(self, sub))]
    pub async fn mount_space(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
        slug: &str,
    ) -> Result<Space, ProjectError> {
        sub.can(AuthVerb::Update, AuthResource::Project(Some(project_id)))?;
        Audit::record_action_if_unset("project.mount_space");
        Audit::record_project_id(project_id);

        let space = self
            .library
            .find_space_by_slug(slug)
            .await?
            .ok_or_else(|| SpaceError::NotFound {
                slug: slug.to_string(),
            })?;

        let mut op = self.repo.begin_op().await?;
        self.mount_space_internal_in_op(&mut op, project_id, space.id)
            .await?;
        op.commit().await?;
        Ok(space)
    }

    /// Creates a new library space and mounts it onto `project_id`.
    /// The space (including the upstream `.gitkeep` push) is atomic
    /// inside `Library::create_space`; the mount runs in a separate
    /// transaction. If the mount fails the space exists library-wide
    /// and the caller can retry via `mount_space` — no dangling state.
    #[instrument(name = "domain.project.create_and_mount_space", skip(self, sub))]
    pub async fn create_and_mount_space(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
        slug: impl Into<String> + std::fmt::Debug,
        description: Option<String>,
    ) -> Result<Space, ProjectError> {
        sub.can(AuthVerb::Update, AuthResource::Project(Some(project_id)))?;
        Audit::record_action_if_unset("project.create_and_mount_space");
        Audit::record_project_id(project_id);

        let space = self.library.create_space(sub, slug, description).await?;
        Audit::record_space_id(space.id);

        let mut op = self.repo.begin_op().await?;
        self.mount_space_internal_in_op(&mut op, project_id, space.id)
            .await?;
        op.commit().await?;
        Ok(space)
    }

    /// Trusted-caller variant: skips the project-admin check. Used
    /// by the public `mount_space` / `create_and_mount_space` wrappers
    /// after they have authorised the broader operation.
    #[instrument(name = "domain.project.mount_space_internal_in_op", skip(self, op))]
    async fn mount_space_internal_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        project_id: ProjectId,
        space_id: SpaceId,
    ) -> Result<(), ProjectError> {
        let mut project = self.repo.find_by_id_in_op(&mut *op, project_id).await?;
        if project.mount_space(space_id).did_execute() {
            self.repo.update_in_op(&mut *op, &mut project).await?;
        }
        Ok(())
    }

    /// Spaces visible to `project_id` — hydrated from `Library`. Soft-
    /// deleted spaces silently drop out of the result.
    #[instrument(name = "domain.project.list_mounted_spaces", skip(self, sub))]
    pub async fn list_mounted_spaces(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
    ) -> Result<Vec<Space>, ProjectError> {
        sub.can(AuthVerb::Read, AuthResource::Project(Some(project_id)))?;
        Audit::record_action_if_unset("project.list_mounted_spaces");
        Audit::record_project_id(project_id);

        let project = self.repo.find_by_id(project_id).await?;
        if project.mounted_spaces.is_empty() {
            return Ok(Vec::new());
        }
        Ok(self
            .library
            .find_spaces_by_ids(&project.mounted_spaces)
            .await?)
    }

    /// Central authorization primitive for sandboxless space access.
    /// Resolves `slug` via `Library`, asserts the calling subject's
    /// project has it mounted, and returns the resolved `Space` for
    /// downstream tracing/audit. Used by `SpaceFs` read ops.
    #[instrument(name = "domain.project.space_for_subject", skip(self, sub))]
    pub async fn space_for_subject(
        &self,
        sub: &AuthSubject,
        slug: &str,
    ) -> Result<Space, ProjectError> {
        let project_id = sub
            .project_id()
            .ok_or(crate::auth::error::AuthorizationError::AuthenticationRequired)?;

        let space = self
            .library
            .find_space_by_slug(slug)
            .await?
            .ok_or_else(|| SpaceError::NotFound {
                slug: slug.to_string(),
            })?;

        let project = self.repo.find_by_id(project_id).await?;
        if !project.is_space_mounted(space.id) {
            return Err(SpaceError::NotMounted {
                slug: space.slug.clone(),
                project_id,
            }
            .into());
        }
        Ok(space)
    }
}

fn build_new_project(
    lead_agent_id: AgentId,
    name: impl Into<String>,
    description: Option<String>,
) -> NewProject {
    let mut builder = NewProject::builder();
    builder.lead_agent_id(lead_agent_id);
    builder.name(name);
    if let Some(desc) = description {
        builder.description(desc);
    }
    builder.build().expect("Could not build new project")
}
