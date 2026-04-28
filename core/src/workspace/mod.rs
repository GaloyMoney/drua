mod entity;
pub mod error;
pub(crate) mod repo;

use std::sync::Arc;

use es_entity::*;
use tracing::instrument;

pub use crate::workspace::workspace_cursor::WorkspaceByCreatedAtCursor;
pub use entity::Workspace;
use entity::*;
pub use error::*;
use repo::*;

use crate::agent::Agents;
use crate::audit::Audit;
use crate::library::Library;
use crate::note::Notes;
use crate::primitives::*;
use crate::sandbox::Sandboxes;
use crate::skill::Skills;
use crate::workspace_secret::WorkspaceSecrets;

#[derive(Clone)]
pub struct Workspaces {
    repo: WorkspaceRepo,
    agents: Arc<Agents>,
    sandboxes: Arc<Sandboxes>,
    skills: Arc<Skills>,
    notes: Arc<Notes>,
    secrets: WorkspaceSecrets,
    library: Library,
}

impl Workspaces {
    pub fn new(
        pool: &sqlx::PgPool,
        agents: Arc<Agents>,
        sandboxes: Arc<Sandboxes>,
        skills: Arc<Skills>,
        notes: Arc<Notes>,
        secrets: WorkspaceSecrets,
        library: Library,
    ) -> Self {
        let repo = WorkspaceRepo::new(pool);
        Self {
            repo,
            agents,
            sandboxes,
            skills,
            notes,
            secrets,
            library,
        }
    }

    #[instrument(name = "domain.workspace.create", skip(self))]
    pub async fn create(
        &self,
        sub: &AuthSubject,
        name: impl Into<String> + std::fmt::Debug,
        description: Option<String>,
    ) -> Result<Workspace, WorkspaceError> {
        sub.can(AuthVerb::Create, AuthResource::Workspace(None))?;
        Audit::record_action_if_unset("workspace.create");
        let name = name.into();
        let lead_agent_id = AgentId::new();
        let new_workspace = build_new_workspace(lead_agent_id, &name, description);
        let workspace_id = new_workspace.id;

        let mut op = self.repo.begin_op().await?;
        let workspace = self.repo.create_in_op(&mut op, new_workspace).await?;

        Audit::record_workspace_id(workspace_id);
        Audit::record_agent_id(lead_agent_id);

        self.agents
            .create_workspace_lead_in_op(&mut op, lead_agent_id, workspace_id, "lead", &name)
            .await?;

        self.library
            .sync_workspace_folder_in_op(&mut op, &name)
            .await?;

        op.commit().await?;

        tracing::info!(
            workspace.id = %workspace.id,
            sub = ?sub,
            "workspace created"
        );

        Ok(workspace)
    }

    #[instrument(name = "domain.workspace.find_by_id", skip(self, sub))]
    pub async fn find_by_id(
        &self,
        sub: &AuthSubject,
        id: impl Into<WorkspaceId> + std::fmt::Debug,
    ) -> Result<Workspace, WorkspaceError> {
        let id = id.into();
        sub.can(AuthVerb::Read, AuthResource::Workspace(Some(id)))?;
        Audit::record_action_if_unset("workspace.find_by_id");
        Audit::record_workspace_id(id);
        Ok(self.repo.find_by_id(id).await?)
    }

    #[instrument(name = "domain.workspace.list", skip(self, sub))]
    pub async fn list(
        &self,
        sub: &AuthSubject,
        query: PaginatedQueryArgs<WorkspaceByCreatedAtCursor>,
        direction: ListDirection,
    ) -> Result<PaginatedQueryRet<Workspace, WorkspaceByCreatedAtCursor>, WorkspaceError> {
        sub.can(AuthVerb::Read, AuthResource::Workspace(None))?;
        Audit::record_action_if_unset("workspace.list");
        Ok(self.repo.list_by_created_at(query, direction).await?)
    }

    #[instrument(name = "domain.workspace.list_all", skip(self, sub))]
    pub async fn list_all(&self, sub: &AuthSubject) -> Result<Vec<Workspace>, WorkspaceError> {
        sub.can(AuthVerb::Read, AuthResource::Workspace(None))?;
        Audit::record_action_if_unset("workspace.list_all");
        Ok(self.repo.list_all().await?)
    }

    #[instrument(name = "domain.workspace.update", skip(self, sub))]
    pub async fn update(
        &self,
        sub: &AuthSubject,
        id: impl Into<WorkspaceId> + std::fmt::Debug,
        description: Option<String>,
    ) -> Result<Workspace, WorkspaceError> {
        let id = id.into();
        sub.can(AuthVerb::Update, AuthResource::Workspace(Some(id)))?;
        Audit::record_action_if_unset("workspace.update");
        Audit::record_workspace_id(id);
        // Load + update in the same op so a concurrent update or delete
        // can't slip in between and produce a lost write.
        let mut op = self.repo.begin_op().await?;
        let mut workspace = self.repo.find_by_id_in_op(&mut op, id).await?;
        workspace.update(description);
        self.repo.update_in_op(&mut op, &mut workspace).await?;
        op.commit().await?;
        Ok(workspace)
    }

    #[instrument(name = "domain.workspace.delete", skip(self, sub))]
    pub async fn delete(
        &self,
        sub: &AuthSubject,
        id: impl Into<WorkspaceId> + std::fmt::Debug,
    ) -> Result<Workspace, WorkspaceError> {
        let id = id.into();
        sub.can(AuthVerb::Delete, AuthResource::Workspace(Some(id)))?;
        Audit::record_action_if_unset("workspace.delete");
        Audit::record_workspace_id(id);
        let mut op = self.repo.begin_op().await?;
        let workspace = self.delete_in_op(&mut op, sub, id).await?;
        op.commit().await?;
        Ok(workspace)
    }

    #[instrument(name = "domain.workspace.delete_in_op", skip(self, op, _sub))]
    pub async fn delete_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        _sub: &AuthSubject,
        id: impl Into<WorkspaceId> + std::fmt::Debug,
    ) -> Result<Workspace, WorkspaceError> {
        let id = id.into();
        // Load through the cascade op so the workspace state we observe
        // here matches what the rest of this transaction operates on.
        let mut workspace = self.repo.find_by_id_in_op(&mut *op, id).await?;

        if !workspace.archive().did_execute() {
            return Ok(workspace);
        }

        // ── 1. Suspend all workspace sandboxes ─────────────────────────
        // Tear down pods/processes via the admin client before touching
        // any DB state so in-flight sandbox work stops.
        if let Err(e) = self.sandboxes.suspend_for_workspace_in_op(op, id).await {
            tracing::warn!(error = %e, "failed to suspend sandboxes during workspace delete");
        }

        // ── 2. Cascade soft-delete agents (→ sessions → threads) ───────
        // Use the op-aware listing so agents created concurrently with
        // this transaction don't escape the cascade (the listing snapshot
        // matches the rest of the cascade's view).
        let agent_list = self.agents.list_for_workspace_in_op(op, id).await?;
        for agent in &agent_list {
            if let Err(e) = self.agents.delete_in_op(op, agent.id).await {
                tracing::warn!(
                    agent_id = %agent.id,
                    error = %e,
                    "failed to delete agent during workspace delete"
                );
            }
        }

        // ── 3. Cascade soft-delete workspace-scoped skills ─────────────
        if let Err(e) = self.skills.delete_for_workspace_in_op(op, id).await {
            tracing::warn!(error = %e, "failed to delete skills during workspace delete");
        }

        // ── 4. Cascade soft-delete workspace notes ─────────────────────
        if let Err(e) = self.notes.delete_for_workspace_in_op(op, id).await {
            tracing::warn!(error = %e, "failed to delete notes during workspace delete");
        }

        // ── 5. Cascade soft-delete workspace secrets ───────────────────
        if let Err(e) = self.secrets.delete_for_workspace_in_op(op, id).await {
            tracing::warn!(error = %e, "failed to delete secrets during workspace delete");
        }

        // ── 6. Clean up library search data + queue git dir removal ────
        if let Err(e) = self
            .library
            .cleanup_workspace_in_op(op, uuid::Uuid::from(id), &workspace.name)
            .await
        {
            tracing::warn!(error = %e, "failed to clean up library during workspace delete");
        }

        // ── 7. Archive + soft-delete the workspace entity itself ───────
        // Persist the archive event first; the second load is reused
        // because `delete_in_op` moves the entity. Both operations go
        // through `op` so the read-write sequence is serialized with any
        // concurrent workspace updates.
        self.repo.update_in_op(&mut *op, &mut workspace).await?;
        let to_delete = self.repo.find_by_id_in_op(&mut *op, id).await?;
        self.repo.delete_in_op(op, to_delete).await?;
        Ok(workspace)
    }

    /// Look up a workspace by name (internal, no auth check).
    /// Returns `Ok(None)` when no workspace matches.
    #[instrument(name = "domain.workspace.find_by_name", skip(self))]
    pub(crate) async fn find_by_name(
        &self,
        name: &str,
    ) -> Result<Option<Workspace>, WorkspaceError> {
        Ok(self.repo.maybe_find_by_name(name).await?)
    }
}

fn build_new_workspace(
    lead_agent_id: AgentId,
    name: impl Into<String>,
    description: Option<String>,
) -> NewWorkspace {
    let mut builder = NewWorkspace::builder();
    builder.lead_agent_id(lead_agent_id);
    builder.name(name);
    if let Some(desc) = description {
        builder.description(desc);
    }
    builder.build().expect("Could not build new workspace")
}
