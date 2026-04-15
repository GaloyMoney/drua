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

/// How long [`Sandboxes::spawn_sandbox_creation`] waits for the admin
/// backend to report the sandbox as ready before giving up.
const PROVISION_TIMEOUT: Duration = Duration::from_secs(120);

pub use config::{SandboxBackendConfig, SandboxConfig};
pub use entity::{NewSandbox, Sandbox, SandboxAgentMode, SandboxEvent, SandboxState};
pub use error::*;
use repo::*;

use crate::primitives::*;

/// Service for managing sandbox lifecycle. Wraps a backend-agnostic
/// [`AdminClient`] (k8s or local) and persists per-sandbox lifecycle state
/// in the `sandboxes` table. Optionally holds a [`GitHubAppTokenProvider`]
/// used to mint a fresh installation token for each `/initialize` call
/// (so the sandbox can clone private repos).
#[derive(Clone)]
pub struct Sandboxes {
    repo: SandboxRepo,
    admin: Arc<dyn AdminClient>,
    github_app: Option<GitHubAppTokenProvider>,
}

impl Sandboxes {
    pub async fn init(
        pool: &sqlx::PgPool,
        config: SandboxConfig,
        github_app: Option<GitHubAppTokenProvider>,
    ) -> Result<Self, SandboxError> {
        let admin: Arc<dyn AdminClient> = match config.backend {
            SandboxBackendConfig::Local { sandbox_spawn_cmd } => Arc::new(LocalAdminClient::new(
                LocalSandboxConfig { sandbox_spawn_cmd },
                &config.local_repo_root,
            )),
            SandboxBackendConfig::K8s {
                namespace,
                template_name,
            } => Arc::new(K8sAdminClient::try_from_env(namespace, template_name).await?),
        };
        Ok(Self {
            repo: SandboxRepo::new(pool),
            admin,
            github_app,
        })
    }

    #[instrument(name = "domain.sandbox.create", skip(self))]
    pub async fn create(
        &self,
        workspace_id: impl Into<WorkspaceId> + std::fmt::Debug,
        name: impl Into<String> + std::fmt::Debug,
        specs: SandboxSpecs,
        mode: SandboxMode,
    ) -> Result<Sandbox, SandboxError> {
        let mut op = self.repo.begin_op().await?;
        let sandbox = self
            .create_in_op(&mut op, workspace_id, name, specs, mode)
            .await?;
        op.commit().await?;
        // Hand off the slow remote work (admin create → wait ready → call
        // /initialize → state transitions) to a background task so callers
        // get a Provisioning sandbox back immediately.
        self.spawn_sandbox_creation(sandbox.id);
        Ok(sandbox)
    }

    /// Composable variant of [`Self::create`]. Persists a new sandbox in
    /// [`SandboxState::Provisioning`]; the admin/instance work is **not**
    /// performed here. Callers that compose this into a larger op are
    /// responsible for invoking [`Self::spawn_sandbox_creation`] after
    /// their outer op commits.
    #[instrument(name = "domain.sandbox.create_in_op", skip(self, op))]
    pub async fn create_in_op(
        &self,
        op: &mut DbOp<'_>,
        workspace_id: impl Into<WorkspaceId> + std::fmt::Debug,
        name: impl Into<String> + std::fmt::Debug,
        specs: SandboxSpecs,
        mode: SandboxMode,
    ) -> Result<Sandbox, SandboxError> {
        let new_sandbox = NewSandbox::builder()
            .workspace_id(workspace_id.into())
            .name(name.into())
            .specs(specs)
            .mode(mode)
            .build()
            .expect("could not build new sandbox");

        let sandbox = self.repo.create_in_op(op, new_sandbox).await?;
        Audit::record_sandbox_id(sandbox.id);
        Ok(sandbox)
    }

    /// Spawn the background lifecycle for a freshly persisted sandbox:
    ///
    /// 1. `admin.create_sandbox(name, specs)`
    /// 2. `admin.wait_sandbox_ready(name, PROVISION_TIMEOUT)` →
    ///    transition entity to [`SandboxState::Initializing`]
    /// 3. `instance.initialize(mode)` → transition to
    ///    [`SandboxState::Ready`]
    ///
    /// Any failure routes through [`Self::record_error`] which transitions
    /// the entity to [`SandboxState::Errored`] and persists the failing
    /// step + reason as a [`SandboxEvent::ProvisioningFailed`]. The UI
    /// surfaces `last_error` whenever `state == Errored`.
    pub fn spawn_sandbox_creation(&self, id: SandboxId) {
        let me = self.clone();
        tokio::spawn(async move {
            me.run_creation_lifecycle(id).await;
        });
    }

    /// Background lifecycle. `#[instrument]` gives the spawned task its
    /// own root span so the per-step `tracing::error!`s land in Honeycomb
    /// instead of disappearing into stderr (the `tokio::spawn` detaches
    /// from the caller's span context).
    #[instrument(
        name = "domain.sandbox.run_creation_lifecycle",
        skip(self),
        fields(sandbox_id = %id, sandbox_name)
    )]
    async fn run_creation_lifecycle(&self, id: SandboxId) {
        let sandbox = match self.repo.find_by_id(id).await {
            Ok(s) => s,
            Err(e) => {
                // No name yet, so just record the error against the id.
                tracing::error!(sandbox_id = %id, error = %e, "could not load sandbox for lifecycle");
                return;
            }
        };
        // Use the entity-id-derived `resource_name` (e.g. `sb-019d…`) for
        // the admin client so the K8s CR / local sandbox dir is uniquely
        // named. The user-facing `sandbox.name` stays in the entity for
        // display only.
        let name = sandbox.resource_name();
        tracing::Span::current().record("sandbox_name", name.as_str());

        // Always attempt to delete first so the lifecycle is idempotent:
        // re-running it (restart / upgrade / retry-after-failure) wipes
        // any half-baked CR/pod from a previous attempt before we
        // re-create. PVCs / local workspace dirs survive delete by
        // design, so workspace state is preserved across the cycle.
        // `NotFound` is the expected case on first-time create — ignore.
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
        // Mint a fresh GitHub App installation token for this initialize
        // call when the provider is configured. Without it, `/initialize`
        // can't `git clone` private repos. Token failures are surfaced
        // through the same `record_error` path as other lifecycle steps.
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
        let mut sandbox = self.repo.find_by_id(id).await?;
        if sandbox.initialized(response).did_execute() {
            self.repo.update_in_op(&mut op, &mut sandbox).await?;
        }
        op.commit().await?;
        Ok(())
    }

    /// Idempotent transition into [`SandboxState::Initializing`].
    async fn run_initializing(&self, id: SandboxId) -> Result<(), SandboxError> {
        let mut op = self.repo.begin_op().await?;
        let mut sandbox = self.repo.find_by_id(id).await?;
        if sandbox.initializing().did_execute() {
            self.repo.update_in_op(&mut op, &mut sandbox).await?;
        }
        op.commit().await?;
        Ok(())
    }

    /// Persist a provisioning failure: transition to `Errored`, push a
    /// `ProvisioningFailed` event with `step` + `reason`, and emit a
    /// matching `tracing::error!` so the failure shows up in Honeycomb.
    /// Failures inside this method itself are logged and otherwise
    /// swallowed — there's nothing useful to do beyond that since the
    /// caller is the lifecycle background task.
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
        let mut sandbox = self.repo.find_by_id(id).await?;
        if sandbox.errored(step, reason).did_execute() {
            self.repo.update_in_op(&mut op, &mut sandbox).await?;
        }
        op.commit().await?;
        Ok(())
    }

    #[instrument(name = "domain.sandbox.find_by_id", skip(self))]
    pub async fn find_by_id(
        &self,
        id: impl Into<SandboxId> + std::fmt::Debug,
    ) -> Result<Sandbox, SandboxError> {
        Ok(self.repo.find_by_id(id.into()).await?)
    }

    /// Resolve a live [`InstanceClient`] for the sandbox identified by `id`.
    ///
    /// Loads the entity, asks the admin client for the current
    /// `base_url`, and wraps it. Returns [`SandboxError::NotReady`] when
    /// the entity isn't in [`SandboxState::Ready`] (the only state where
    /// a `base_url` is guaranteed). Used by tools that act inside the
    /// sandbox (e.g. the `bash` top-level tool).
    #[instrument(name = "domain.sandbox.instance_client_for", skip(self))]
    pub async fn instance_client_for(
        &self,
        id: impl Into<SandboxId> + std::fmt::Debug,
    ) -> Result<InstanceClient, SandboxError> {
        let id = id.into();
        let sandbox = self.repo.find_by_id(id).await?;
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

    #[instrument(name = "domain.sandbox.list_for_workspace", skip(self))]
    pub async fn list_for_workspace(
        &self,
        workspace_id: impl Into<WorkspaceId> + std::fmt::Debug,
    ) -> Result<Vec<Sandbox>, SandboxError> {
        const PAGE_SIZE: usize = 100;
        let workspace_id = workspace_id.into();
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

    /// Bring a [`SandboxState::Suspended`] sandbox back to life. Transitions
    /// the entity to `Provisioning` and re-runs the creation lifecycle:
    /// admin recreates the pod/process (reusing the retained PVC / local
    /// workspace dir), then `/initialize` is called again. The server's
    /// `/initialize` is idempotent — it overwrites the GitHub token and
    /// skips re-cloning when the repo is already on disk.
    #[instrument(name = "domain.sandbox.restart", skip(self))]
    pub async fn restart(
        &self,
        id: impl Into<SandboxId> + std::fmt::Debug,
    ) -> Result<Sandbox, SandboxError> {
        let id = id.into();
        Audit::record_sandbox_id(id);
        let mut op = self.repo.begin_op().await?;
        let mut sandbox = self.repo.find_by_id(id).await?;
        if sandbox.provisioning().did_execute() {
            self.repo.update_in_op(&mut op, &mut sandbox).await?;
        }
        op.commit().await?;
        // Re-use the same background lifecycle the initial create goes
        // through — admin.create_sandbox → wait_ready → /initialize → Ready.
        self.spawn_sandbox_creation(id);
        Ok(sandbox)
    }

    /// Attach `agent_id` to the sandbox in `mode`. Verifies the sandbox
    /// belongs to `workspace_id` (else returns
    /// [`SandboxError::WrongWorkspace`]) and delegates to
    /// [`Sandbox::attach_agent`] which enforces single-writer.
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
        let mut sandbox = self.repo.find_by_id(sandbox_id).await?;
        if sandbox
            .attach_agent(agent_id, workspace_id, mode)?
            .did_execute()
        {
            self.repo.update_in_op(op, &mut sandbox).await?;
        }
        Ok(sandbox)
    }

    /// Detach `agent_id` from the sandbox. Idempotent at the entity level
    /// (no-op if not attached).
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
        let mut sandbox = self.repo.find_by_id(sandbox_id).await?;
        if sandbox.detach_agent(agent_id).did_execute() {
            self.repo.update_in_op(op, &mut sandbox).await?;
        }
        Ok(sandbox)
    }

    #[instrument(name = "domain.sandbox.suspend", skip(self))]
    pub async fn suspend(
        &self,
        id: impl Into<SandboxId> + std::fmt::Debug,
    ) -> Result<Sandbox, SandboxError> {
        let mut op = self.repo.begin_op().await?;
        let sandbox = self.suspend_in_op(&mut op, id).await?;
        op.commit().await?;
        Ok(sandbox)
    }

    /// Composable variant of [`Self::suspend`]. Tears down the underlying
    /// container/process via the admin client and transitions the entity
    /// to [`SandboxState::Suspended`] (the entity is retained — call
    /// `create` again to recreate). The admin call is best-effort: if it
    /// fails we still record the state transition with a warning so callers
    /// can retry/reconcile.
    #[instrument(name = "domain.sandbox.suspend_in_op", skip(self, op))]
    pub async fn suspend_in_op(
        &self,
        op: &mut DbOp<'_>,
        id: impl Into<SandboxId> + std::fmt::Debug,
    ) -> Result<Sandbox, SandboxError> {
        let id = id.into();
        Audit::record_sandbox_id(id);
        let mut sandbox = self.repo.find_by_id(id).await?;

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
