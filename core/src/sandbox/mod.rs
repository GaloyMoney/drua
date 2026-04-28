mod config;
mod entity;
pub mod error;
pub(crate) mod repo;

use std::sync::Arc;
use std::time::Duration;

use es_entity::*;
use tracing::instrument;

use sandbox::admin_client::{AdminClient, K8sAdminClient, LocalAdminClient, LocalSandboxConfig};
use sandbox::instance_client::{InitializeRequest, InitializeResponse, InstanceClient};
pub use sandbox::{SandboxMode, SandboxSpecs};

use crate::audit::Audit;
use crate::github_app::GitHubAppTokenProvider;

const PROVISION_TIMEOUT: Duration = Duration::from_secs(120);

pub use config::{SandboxBackendConfig, SandboxConfig};
pub use entity::{NewSandbox, Sandbox, SandboxAgentMode, SandboxEvent, SandboxState};
pub use error::*;
use repo::*;

use crate::primitives::*;

use crate::auth::AuthSubject;

#[derive(Clone)]
pub struct Sandboxes {
    repo: SandboxRepo,
    admin: Arc<dyn AdminClient>,
    github_app: Option<Arc<GitHubAppTokenProvider>>,
}

impl Sandboxes {
    pub async fn init(
        pool: &sqlx::PgPool,
        config: SandboxConfig,
        github_app: Option<Arc<GitHubAppTokenProvider>>,
    ) -> Result<Self, SandboxError> {
        let admin: Arc<dyn AdminClient> = match config.backend {
            SandboxBackendConfig::Local {
                sandbox_spawn_cmd,
                local_repo_root,
            } => Arc::new(LocalAdminClient::new(
                LocalSandboxConfig { sandbox_spawn_cmd },
                &local_repo_root,
            )),
            SandboxBackendConfig::K8s {
                namespace,
                template_name,
                storage_class,
                mount_path,
            } => {
                let mut client = K8sAdminClient::try_from_env(namespace, template_name).await?;
                // PVC-backed mount; without it pods use ephemeral storage.
                if let (Some(sc), Some(mp)) = (storage_class, mount_path) {
                    client =
                        client.with_persistence(sandbox::admin_client::k8s::PersistenceConfig {
                            storage_class: sc,
                            mount_path: mp,
                        });
                }
                Arc::new(client)
            }
        };
        Ok(Self {
            repo: SandboxRepo::new(pool),
            admin,
            github_app,
        })
    }

    #[instrument(name = "domain.sandbox.create", skip(self, sub))]
    pub async fn create(
        &self,
        sub: &AuthSubject,
        workspace_id: impl Into<WorkspaceId> + std::fmt::Debug,
        name: impl Into<String> + std::fmt::Debug,
        specs: SandboxSpecs,
        mode: SandboxMode,
    ) -> Result<Sandbox, SandboxError> {
        let workspace_id = workspace_id.into();
        sub.can(AuthVerb::Create, AuthResource::Sandbox(workspace_id, None))?;
        Audit::record_action_if_unset("sandbox.create");
        Audit::record_workspace_id(workspace_id);
        let mut op = self.repo.begin_op().await?;
        let sandbox = self
            .create_in_op(&mut op, workspace_id, name, specs, mode)
            .await?;
        op.commit().await?;
        // Background task so callers get a Provisioning sandbox immediately.
        self.spawn_sandbox_creation(sandbox.id);
        Ok(sandbox)
    }

    /// Persists in `Provisioning`; caller must invoke `spawn_sandbox_creation`
    /// after the outer op commits.
    #[instrument(name = "domain.sandbox.create_in_op", skip(self, op))]
    pub async fn create_in_op(
        &self,
        op: &mut DbOp<'_>,
        workspace_id: impl Into<WorkspaceId> + std::fmt::Debug,
        name: impl Into<String> + std::fmt::Debug,
        specs: SandboxSpecs,
        mode: SandboxMode,
    ) -> Result<Sandbox, SandboxError> {
        let id = SandboxId::new();
        let mount_path = self.admin.mount_path(&format!("sb-{id}"));
        let new_sandbox = NewSandbox::builder()
            .id(id)
            .workspace_id(workspace_id.into())
            .name(name.into())
            .specs(specs)
            .mode(mode)
            .mount_path(mount_path)
            .build()
            .expect("could not build new sandbox");

        let sandbox = self.repo.create_in_op(op, new_sandbox).await?;
        Audit::record_sandbox_id(sandbox.id);
        Ok(sandbox)
    }

    /// Lifecycle: create → wait_ready (→ Initializing) → initialize (→ Ready).
    /// Failures route through `record_error` → `Errored` + `ProvisioningFailed`.
    pub fn spawn_sandbox_creation(&self, id: SandboxId) {
        let me = self.clone();
        tokio::spawn(async move {
            me.run_creation_lifecycle(id).await;
        });
    }

    /// `#[instrument]` gives the spawned task its own root span so per-step
    /// `tracing::error!`s land in Honeycomb (tokio::spawn detaches the span).
    #[instrument(
        name = "domain.sandbox.run_creation_lifecycle",
        skip(self),
        fields(sandbox_id = %id, sandbox_name)
    )]
    async fn run_creation_lifecycle(&self, id: SandboxId) {
        let sandbox = match self.repo.find_by_id(id).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(sandbox_id = %id, error = %e, "could not load sandbox for lifecycle");
                return;
            }
        };
        // resource_name (`sb-019d…`) is unique; sandbox.name is display-only.
        let name = sandbox.resource_name();
        tracing::Span::current().record("sandbox_name", name.as_str());

        // Pre-create delete makes lifecycle idempotent. PVCs/workspace dirs
        // survive delete by design. `NotFound` is the first-create case.
        match self.admin.delete_sandbox(&name).await {
            Ok(()) => {
                tracing::info!(sandbox = %name, "pre-create delete: removed existing sandbox")
            }
            Err(sandbox::AdminError::NotFound(_)) => {}
            Err(e) => {
                self.record_error(id, &name, "pre_create_delete", e.to_string())
                    .await;
                return;
            }
        }

        if let Err(e) = self.admin.create_sandbox(&name, &sandbox.specs).await {
            self.record_error(id, &name, "create_sandbox", e.to_string())
                .await;
            return;
        }

        let view = match self
            .admin
            .wait_sandbox_ready(&name, PROVISION_TIMEOUT)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                self.record_error(id, &name, "wait_sandbox_ready", e.to_string())
                    .await;
                return;
            }
        };

        if let Err(e) = self.run_initializing(id).await {
            self.record_error(id, &name, "transition_initializing", e.to_string())
                .await;
            return;
        }

        let Some(base_url) = view.base_url else {
            self.record_error(
                id,
                &name,
                "wait_sandbox_ready",
                "sandbox reported ready but admin returned no base_url".to_string(),
            )
            .await;
            return;
        };
        let instance = InstanceClient::new(base_url);
        // Without the token, `/initialize` can't clone private repos.
        let github_token = match self.github_app.as_ref() {
            Some(provider) => match provider.generate_token().await {
                Ok(t) => Some(t.token),
                Err(e) => {
                    self.record_error(id, &name, "github_app_token", e.to_string())
                        .await;
                    return;
                }
            },
            None => None,
        };
        let init_req = InitializeRequest::from_mode(&sandbox.mode, github_token);
        let response = match instance.initialize(&init_req).await {
            Ok(r) => r,
            Err(e) => {
                self.record_error(id, &name, "initialize", e.to_string())
                    .await;
                return;
            }
        };

        if let Err(e) = self.apply_initialized(id, &response).await {
            self.record_error(id, &name, "apply_initialized", e.to_string())
                .await;
        }
    }

    async fn apply_initialized(
        &self,
        id: SandboxId,
        response: &InitializeResponse,
    ) -> Result<(), SandboxError> {
        let mut op = self.repo.begin_op().await?;
        let mut sandbox = self.repo.find_by_id_in_op(&mut op, id).await?;
        if sandbox.initialized(response).did_execute() {
            self.repo.update_in_op(&mut op, &mut sandbox).await?;
        }
        op.commit().await?;
        Ok(())
    }

    async fn run_initializing(&self, id: SandboxId) -> Result<(), SandboxError> {
        let mut op = self.repo.begin_op().await?;
        let mut sandbox = self.repo.find_by_id_in_op(&mut op, id).await?;
        if sandbox.initializing().did_execute() {
            self.repo.update_in_op(&mut op, &mut sandbox).await?;
        }
        op.commit().await?;
        Ok(())
    }

    /// Transition to `Errored`, push `ProvisioningFailed`, log error.
    /// Inner failures are logged and swallowed — no useful recovery path.
    async fn record_error(&self, id: SandboxId, name: &str, step: &'static str, reason: String) {
        tracing::error!(sandbox = %name, step, error = %reason, "sandbox provisioning step failed");
        if let Err(e) = self.persist_errored(id, step, &reason).await {
            tracing::error!(sandbox = %name, step, error = %e, "failed to persist provisioning error");
        }
    }

    async fn persist_errored(
        &self,
        id: SandboxId,
        step: &str,
        reason: &str,
    ) -> Result<(), SandboxError> {
        let mut op = self.repo.begin_op().await?;
        let mut sandbox = self.repo.find_by_id_in_op(&mut op, id).await?;
        if sandbox.errored(step, reason).did_execute() {
            self.repo.update_in_op(&mut op, &mut sandbox).await?;
        }
        op.commit().await?;
        Ok(())
    }

    #[instrument(name = "domain.sandbox.find_by_id", skip(self, sub))]
    pub async fn find_by_id(
        &self,
        sub: &AuthSubject,
        id: impl Into<SandboxId> + std::fmt::Debug,
    ) -> Result<Sandbox, SandboxError> {
        let sandbox = self.repo.find_by_id(id.into()).await?;
        sub.can(
            AuthVerb::Read,
            AuthResource::Sandbox(sandbox.workspace_id, Some(sandbox.id)),
        )?;
        Audit::record_action_if_unset("sandbox.find_by_id");
        Audit::record_workspace_id(sandbox.workspace_id);
        Audit::record_sandbox_id(sandbox.id);
        Ok(sandbox)
    }

    /// `Ok(None)` instead of error when not found.
    #[instrument(name = "domain.sandbox.maybe_find_by_id", skip(self))]
    pub async fn maybe_find_by_id(
        &self,
        id: impl Into<SandboxId> + std::fmt::Debug,
    ) -> Result<Option<Sandbox>, SandboxError> {
        Ok(self.repo.maybe_find_by_id(id.into()).await?)
    }

    /// Internal lookup for the workflow executor — no auth subject
    /// because the run was already authorised when triggered.
    #[instrument(name = "domain.sandbox.find_for_workflow_run", skip(self))]
    pub async fn find_for_workflow_run(
        &self,
        workspace_id: WorkspaceId,
        name: &str,
    ) -> Result<Option<Sandbox>, SandboxError> {
        let query = PaginatedQueryArgs {
            first: 100,
            after: None,
        };
        let result = self
            .repo
            .list_for_workspace_id_by_created_at(workspace_id, query, ListDirection::Descending)
            .await?;
        Ok(result.entities.into_iter().find(|s| s.name == name))
    }

    /// `pub(crate)` helper used by the skills context builder; no auth check.
    #[instrument(name = "domain.sandbox.exported_skills_for_workspace", skip(self))]
    pub(crate) async fn exported_skills_for_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<sandbox::instance_client::ExportedSkill>, SandboxError> {
        let query = PaginatedQueryArgs {
            first: 100,
            after: None,
        };
        let result = self
            .repo
            .list_for_workspace_id_by_created_at(workspace_id, query, ListDirection::Descending)
            .await?;
        let mut skills = Vec::new();
        for sandbox in &result.entities {
            if sandbox.state != SandboxState::Ready {
                continue;
            }
            skills.extend(sandbox.exported_skills.iter().cloned());
        }
        Ok(skills)
    }

    /// Returns `NotReady` unless `state == Ready` (only state with `base_url`).
    #[instrument(name = "domain.sandbox.instance_client_for", skip(self, sub))]
    pub async fn instance_client_for(
        &self,
        sub: &AuthSubject,
        id: impl Into<SandboxId> + std::fmt::Debug,
    ) -> Result<InstanceClient, SandboxError> {
        self.instance_client_with_verb(sub, id, AuthVerb::Use).await
    }

    /// For read-only sandbox tools (ls, read, grep, glob).
    #[instrument(name = "domain.sandbox.instance_client_for_read", skip(self, sub))]
    pub async fn instance_client_for_read(
        &self,
        sub: &AuthSubject,
        id: impl Into<SandboxId> + std::fmt::Debug,
    ) -> Result<InstanceClient, SandboxError> {
        self.instance_client_with_verb(sub, id, AuthVerb::Read)
            .await
    }

    async fn instance_client_with_verb(
        &self,
        sub: &AuthSubject,
        id: impl Into<SandboxId> + std::fmt::Debug,
        verb: AuthVerb,
    ) -> Result<InstanceClient, SandboxError> {
        let id = id.into();
        let sandbox = self.repo.find_by_id(id).await?;
        sub.can(
            verb,
            AuthResource::Sandbox(sandbox.workspace_id, Some(sandbox.id)),
        )?;
        Audit::record_action_if_unset("sandbox.instance_client_for");
        Audit::record_workspace_id(sandbox.workspace_id);
        Audit::record_sandbox_id(sandbox.id);
        if sandbox.state != SandboxState::Ready {
            return Err(SandboxError::NotReady {
                state: sandbox.state.to_string(),
            });
        }
        let view = self.admin.get_sandbox(&sandbox.resource_name()).await?;
        InstanceClient::from_sandbox(&view).ok_or_else(|| SandboxError::NotReady {
            state: "ready (no base_url reported)".to_string(),
        })
    }

    #[instrument(name = "domain.sandbox.list_for_workspace", skip(self, sub))]
    pub async fn list_for_workspace(
        &self,
        sub: &AuthSubject,
        workspace_id: impl Into<WorkspaceId> + std::fmt::Debug,
    ) -> Result<Vec<Sandbox>, SandboxError> {
        const PAGE_SIZE: usize = 100;
        let workspace_id = workspace_id.into();
        sub.can(AuthVerb::Read, AuthResource::Sandbox(workspace_id, None))?;
        Audit::record_action_if_unset("sandbox.list_for_workspace");
        Audit::record_workspace_id(workspace_id);
        let mut all = Vec::new();
        let mut query = PaginatedQueryArgs {
            first: PAGE_SIZE,
            after: None,
        };

        loop {
            let mut result = self
                .repo
                .list_for_workspace_id_by_created_at(workspace_id, query, ListDirection::Descending)
                .await?;
            all.append(&mut result.entities);
            match result.into_next_query() {
                Some(next) => query = next,
                None => break,
            }
        }
        Ok(all)
    }

    /// Re-runs the creation lifecycle reusing the retained PVC/workspace dir.
    /// Server `/initialize` is idempotent: overwrites token, skips re-clone.
    #[instrument(name = "domain.sandbox.restart", skip(self, sub))]
    pub async fn restart(
        &self,
        sub: &AuthSubject,
        id: impl Into<SandboxId> + std::fmt::Debug,
    ) -> Result<Sandbox, SandboxError> {
        let id = id.into();
        let pre = self.repo.find_by_id(id).await?;
        sub.can(
            AuthVerb::Update,
            AuthResource::Sandbox(pre.workspace_id, Some(pre.id)),
        )?;
        Audit::record_action_if_unset("sandbox.restart");
        Audit::record_workspace_id(pre.workspace_id);
        Audit::record_sandbox_id(id);
        let mut op = self.repo.begin_op().await?;
        let mut sandbox = self.repo.find_by_id_in_op(&mut op, id).await?;
        if sandbox.provisioning().did_execute() {
            self.repo.update_in_op(&mut op, &mut sandbox).await?;
        }
        op.commit().await?;
        self.spawn_sandbox_creation(id);
        Ok(sandbox)
    }

    /// Verifies workspace match (`WrongWorkspace` else); delegates to
    /// `Sandbox::attach_agent` which enforces single-writer.
    #[instrument(
        name = "domain.sandbox.attach_to_agent_in_op",
        skip(self, op),
        fields(%workspace_id, %sandbox_id, %agent_id, ?mode)
    )]
    pub async fn attach_to_agent_in_op(
        &self,
        op: &mut DbOp<'_>,
        workspace_id: WorkspaceId,
        sandbox_id: SandboxId,
        agent_id: AgentId,
        mode: SandboxAgentMode,
    ) -> Result<Sandbox, SandboxError> {
        Audit::record_sandbox_id(sandbox_id);
        Audit::record_workspace_id(workspace_id);
        Audit::record_agent_id(agent_id);
        let mut sandbox = self.repo.find_by_id_in_op(&mut *op, sandbox_id).await?;
        if sandbox
            .attach_agent(agent_id, workspace_id, mode)?
            .did_execute()
        {
            self.repo.update_in_op(op, &mut sandbox).await?;
        }
        Ok(sandbox)
    }

    /// Idempotent (no-op if not attached).
    #[instrument(
        name = "domain.sandbox.detach_from_agent_in_op",
        skip(self, op),
        fields(%sandbox_id, %agent_id)
    )]
    pub async fn detach_from_agent_in_op(
        &self,
        op: &mut DbOp<'_>,
        sandbox_id: SandboxId,
        agent_id: AgentId,
    ) -> Result<Sandbox, SandboxError> {
        Audit::record_sandbox_id(sandbox_id);
        Audit::record_agent_id(agent_id);
        let mut sandbox = self.repo.find_by_id_in_op(&mut *op, sandbox_id).await?;
        if sandbox.detach_agent(agent_id).did_execute() {
            self.repo.update_in_op(op, &mut sandbox).await?;
        }
        Ok(sandbox)
    }

    /// Best-effort: per-sandbox failures are logged and skipped.
    #[instrument(name = "domain.sandbox.suspend_for_workspace_in_op", skip(self, op))]
    pub async fn suspend_for_workspace_in_op(
        &self,
        op: &mut DbOp<'_>,
        workspace_id: WorkspaceId,
    ) -> Result<(), SandboxError> {
        let query = PaginatedQueryArgs {
            first: 100,
            after: None,
        };
        let result = self
            .repo
            .list_for_workspace_id_by_created_at_in_op(
                &mut *op,
                workspace_id,
                query,
                ListDirection::Descending,
            )
            .await?;
        for sandbox in result.entities {
            if sandbox.state == SandboxState::Suspended {
                continue;
            }
            if let Err(e) = self.suspend_in_op(op, sandbox.id).await {
                tracing::warn!(
                    sandbox_id = %sandbox.id,
                    error = %e,
                    "failed to suspend sandbox during workspace delete"
                );
            }
        }
        Ok(())
    }

    #[instrument(name = "domain.sandbox.suspend", skip(self, sub))]
    pub async fn suspend(
        &self,
        sub: &AuthSubject,
        id: impl Into<SandboxId> + std::fmt::Debug,
    ) -> Result<Sandbox, SandboxError> {
        let id = id.into();
        let sandbox = self.repo.find_by_id(id).await?;
        sub.can(
            AuthVerb::Update,
            AuthResource::Sandbox(sandbox.workspace_id, Some(sandbox.id)),
        )?;
        Audit::record_action_if_unset("sandbox.suspend");
        Audit::record_workspace_id(sandbox.workspace_id);
        let mut op = self.repo.begin_op().await?;
        let sandbox = self.suspend_in_op(&mut op, id).await?;
        op.commit().await?;
        Ok(sandbox)
    }

    /// Admin delete is best-effort: failure logs a warning but still records
    /// the state transition so callers can retry/reconcile.
    #[instrument(name = "domain.sandbox.suspend_in_op", skip(self, op))]
    pub async fn suspend_in_op(
        &self,
        op: &mut DbOp<'_>,
        id: impl Into<SandboxId> + std::fmt::Debug,
    ) -> Result<Sandbox, SandboxError> {
        let id = id.into();
        Audit::record_sandbox_id(id);
        let mut sandbox = self.repo.find_by_id_in_op(&mut *op, id).await?;

        let resource_name = sandbox.resource_name();
        if let Err(e) = self.admin.delete_sandbox(&resource_name).await {
            tracing::warn!(
                sandbox = %resource_name,
                error = %e,
                "Admin client delete_sandbox failed; suspending entity anyway"
            );
        }

        if sandbox.suspended().did_execute() {
            self.repo.update_in_op(op, &mut sandbox).await?;
        }
        Ok(sandbox)
    }
}
