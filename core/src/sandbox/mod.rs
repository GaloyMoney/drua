mod config;
mod entity;
pub mod error;
pub(crate) mod repo;

use std::sync::Arc;
use std::time::Duration;

use es_entity::*;
use tracing::instrument;

use sandbox::admin_client::{AdminClient, K8sAdminClient, LocalAdminClient, LocalSandboxConfig};
use sandbox::instance_client::{
    AttachRequest, InitializeRequest, InitializeResponse, InstanceClient,
};
pub use sandbox::{parse_k8s_quantity, SandboxMode, SandboxSpecs};

use crate::audit::Audit;

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
    /// Same global allow-list the git-proxy uses for runtime decisions.
    /// Sandbox-create pre-validates `mode: repo` against it so we don't
    /// leak an `Errored` sandbox + a half-baked workspace when the
    /// /initialize-time clone would fail.
    allowlist: Arc<drua_git_proxy::Allowlist>,
}

impl Sandboxes {
    pub async fn init(
        pool: &sqlx::PgPool,
        config: SandboxConfig,
        allowlist: Arc<drua_git_proxy::Allowlist>,
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
                nix_store,
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
                if let Some(ns) = nix_store {
                    client = client.with_nix_store(sandbox::admin_client::k8s::NixStoreConfig {
                        storage_class: ns.storage_class,
                        size: ns.size,
                    });
                }
                Arc::new(client)
            }
        };
        Ok(Self {
            repo: SandboxRepo::new(pool),
            admin,
            allowlist,
        })
    }

    /// One-line description of the git-proxy push policy for this
    /// sandbox, surfaced in the attach notification so the agent picks
    /// an accepted branch name on its first push instead of hitting a
    /// `push_ref_denied` after the fact. `None` for scratch sandboxes
    /// or repos absent from the allow-list (the latter is unreachable
    /// from a successful create — `validate_repo_mode` rejects those —
    /// but stay defensive in case the YAML is reloaded with the repo
    /// removed before an old sandbox re-attaches).
    pub fn push_policy_text(&self, mode: &SandboxMode) -> Option<String> {
        let SandboxMode::Repo { repo_url, .. } = mode else {
            return None;
        };
        let coord = drua_git_proxy::RepoCoord::from_github_url(repo_url)?;
        let entry = self.allowlist.lookup(&coord.owner, &coord.repo)?;
        if !entry.modes.contains(&drua_git_proxy::GitProxyMode::Push) {
            return Some(
                "Push policy: this repo is read-only via the git-proxy — pushes are not accepted."
                    .to_string(),
            );
        }
        let patterns = entry.push_refs.raw();
        if patterns.is_empty() {
            return Some(
                "Push policy: no ref patterns are allowed for push (repo is effectively read-only)."
                    .to_string(),
            );
        }
        let list = patterns
            .iter()
            .map(|p| format!("`{p}`"))
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!(
            "Push policy: only these refs are accepted on push: {list}. Other branches will be rejected with `push_ref_denied`."
        ))
    }

    /// Pre-validate `mode: repo` against the global git-proxy allow-list
    /// before persisting the sandbox row. Reject early with a clear
    /// error rather than letting `/initialize` fail mid-clone and
    /// leaving the sandbox in `Errored` (memo `019dfebc` §7.2).
    /// Scratch-mode sandboxes are unaffected.
    fn validate_repo_mode(&self, mode: &SandboxMode) -> Result<(), SandboxError> {
        let SandboxMode::Repo { repo_url, .. } = mode else {
            return Ok(());
        };
        let coord = drua_git_proxy::RepoCoord::from_github_url(repo_url).ok_or_else(|| {
            SandboxError::InvalidRepoUrl {
                url: repo_url.clone(),
            }
        })?;
        // Pull is the floor — the sandbox tool-server runs `git clone`
        // at `/initialize`. Push happens later from the agent and is
        // checked at request time by the proxy.
        self.allowlist
            .check_authorization(
                &coord.owner,
                &coord.repo,
                drua_git_proxy::GitProxyMode::Pull,
                &[],
            )
            .map_err(|e| SandboxError::RepoNotAllowed {
                url: repo_url.clone(),
                reason: e.reject_code().to_string(),
            })?;
        Ok(())
    }

    #[instrument(name = "domain.sandbox.create", skip(self, sub))]
    pub async fn create(
        &self,
        sub: &AuthSubject,
        project_id: impl Into<ProjectId> + std::fmt::Debug,
        name: impl Into<String> + std::fmt::Debug,
        specs: SandboxSpecs,
        mode: SandboxMode,
    ) -> Result<Sandbox, SandboxError> {
        let project_id = project_id.into();
        sub.can(AuthVerb::Create, AuthResource::Sandbox(project_id, None))?;
        Audit::record_action_if_unset("sandbox.create");
        Audit::record_project_id(project_id);
        let mut op = self.repo.begin_op().await?;
        let sandbox = self
            .create_in_op(&mut op, project_id, name, specs, mode)
            .await?;
        op.commit().await?;
        // Background task so callers get a Provisioning sandbox immediately.
        self.spawn_sandbox_creation(sandbox.id);
        Ok(sandbox)
    }

    /// Persists in `Provisioning`. Use [`Self::create`] (or
    /// [`Self::create_for_workflow`]) when you also need the
    /// background provisioning lifecycle spawned; this `_in_op`
    /// variant only writes the row.
    #[instrument(name = "domain.sandbox.create_in_op", skip(self, op))]
    pub async fn create_in_op(
        &self,
        op: &mut DbOp<'_>,
        project_id: impl Into<ProjectId> + std::fmt::Debug,
        name: impl Into<String> + std::fmt::Debug,
        specs: SandboxSpecs,
        mode: SandboxMode,
    ) -> Result<Sandbox, SandboxError> {
        self.validate_repo_mode(&mode)?;
        let id = SandboxId::new();
        let mount_path = self.admin.mount_path(&format!("sb-{id}"));
        let new_sandbox = NewSandbox::builder()
            .id(id)
            .project_id(project_id.into())
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
    fn spawn_sandbox_creation(&self, id: SandboxId) {
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
        // No GitHub token is sent to the sandbox: clones go through the
        // drua git-proxy, authenticated by the sandbox's projected SA
        // token. Memo `019dfebc` M4 — the sandbox no longer holds any
        // GitHub credential.
        let init_req = InitializeRequest::from_mode(&sandbox.mode);
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
            AuthResource::Sandbox(sandbox.project_id, Some(sandbox.id)),
        )?;
        Audit::record_action_if_unset("sandbox.find_by_id");
        Audit::record_project_id(sandbox.project_id);
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

    /// Resolve a project-scoped sandbox by its (project-unique)
    /// name. Workflow-scoped sandboxes are excluded — those embed a
    /// per-workflow suffix in `name` and aren't user-addressable.
    #[instrument(name = "domain.sandbox.find_by_name_in_project", skip(self, sub))]
    pub async fn find_by_name_in_project(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
        name: &str,
    ) -> Result<Sandbox, SandboxError> {
        let sandbox = self.find_by_name_in_project_inner(project_id, name).await?;
        sub.can(
            AuthVerb::Read,
            AuthResource::Sandbox(sandbox.project_id, Some(sandbox.id)),
        )?;
        Audit::record_action_if_unset("sandbox.find_by_name_in_project");
        Audit::record_project_id(sandbox.project_id);
        Audit::record_sandbox_id(sandbox.id);
        Ok(sandbox)
    }

    /// Auth-free counterpart used by the workflow executor at run time.
    /// The run was authorised when triggered.
    #[instrument(name = "domain.sandbox.find_by_name_in_project_unchecked", skip(self))]
    pub(crate) async fn find_by_name_in_project_unchecked(
        &self,
        project_id: ProjectId,
        name: &str,
    ) -> Result<Sandbox, SandboxError> {
        self.find_by_name_in_project_inner(project_id, name).await
    }

    async fn find_by_name_in_project_inner(
        &self,
        project_id: ProjectId,
        name: &str,
    ) -> Result<Sandbox, SandboxError> {
        let query = PaginatedQueryArgs {
            first: 100,
            after: None,
        };
        let result = self
            .repo
            .list_for_name_by_created_at(name.to_string(), query, ListDirection::Ascending)
            .await?;
        result
            .entities
            .into_iter()
            .find(|s| s.project_id == project_id && s.workflow_id.is_none())
            .ok_or_else(|| SandboxError::NameNotFound {
                project_id,
                name: name.to_string(),
            })
    }

    pub async fn begin_op(&self) -> Result<DbOp<'_>, SandboxError> {
        Ok(self.repo.begin_op().await?)
    }

    /// In-op lookup for sister services that already hold an open
    /// `DbOp`. Auth-free; callers must have authorised the enclosing
    /// operation themselves.
    #[instrument(name = "domain.sandbox.find_by_id_in_op", skip(self, op))]
    pub(crate) async fn find_by_id_in_op(
        &self,
        op: &mut DbOp<'_>,
        id: SandboxId,
    ) -> Result<Sandbox, SandboxError> {
        Ok(self.repo.find_by_id_in_op(op, id).await?)
    }

    /// `(project_id, name)` is unique at the DB level, so workflow-
    /// scoped sandboxes embed the workflow short-id in their stored
    /// name. The user's decl name stays the canonical reference inside
    /// the workflow YAML; the storage name is an implementation detail.
    fn workflow_sandbox_storage_name(workflow_id: WorkflowDefinitionId, decl_name: &str) -> String {
        let short = &workflow_id.to_string()[..8];
        format!("{decl_name}-wf-{short}")
    }

    /// Lookup scoped to a single workflow definition. Used by the
    /// executor's pre-flight phase; sandboxes are workflow-scoped so a
    /// name can collide across different workflows.
    #[instrument(name = "domain.sandbox.find_for_workflow", skip(self))]
    pub async fn find_for_workflow(
        &self,
        project_id: ProjectId,
        workflow_id: WorkflowDefinitionId,
        name: &str,
    ) -> Result<Option<Sandbox>, SandboxError> {
        let storage_name = Self::workflow_sandbox_storage_name(workflow_id, name);
        let query = PaginatedQueryArgs {
            first: 100,
            after: None,
        };
        let result = self
            .repo
            .list_for_project_id_by_created_at(project_id, query, ListDirection::Descending)
            .await?;
        Ok(result
            .entities
            .into_iter()
            .find(|s| s.name == storage_name && s.workflow_id == Some(workflow_id)))
    }

    /// Workflow-scoped create + commit + lifecycle spawn. Wraps an
    /// internal `_in_op` step with op begin/commit and the background
    /// provisioning spawn so callers (the workflow executor) get a
    /// `Provisioning` sandbox in one call, mirroring [`Self::create`]
    /// for user-driven sandboxes.
    #[instrument(name = "domain.sandbox.create_for_workflow", skip(self))]
    pub async fn create_for_workflow(
        &self,
        project_id: ProjectId,
        workflow_id: WorkflowDefinitionId,
        name: impl Into<String> + std::fmt::Debug,
        specs: SandboxSpecs,
        mode: SandboxMode,
    ) -> Result<Sandbox, SandboxError> {
        let mut op = self.repo.begin_op().await?;
        let sandbox = self
            .create_for_workflow_in_op(&mut op, project_id, workflow_id, name, specs, mode)
            .await?;
        op.commit().await?;
        self.spawn_sandbox_creation(sandbox.id);
        Ok(sandbox)
    }

    /// Workflow-scoped create; mirrors `create_in_op` but tags the
    /// entity with `workflow_id` and skips the auth check (the workflow
    /// run was authorised when triggered). The stored name is the
    /// decl name plus a workflow short-id suffix so two workflows can
    /// declare the same sandbox name without violating the unique
    /// `(project_id, name)` index.
    #[instrument(name = "domain.sandbox.create_for_workflow_in_op", skip(self, op))]
    async fn create_for_workflow_in_op(
        &self,
        op: &mut DbOp<'_>,
        project_id: ProjectId,
        workflow_id: WorkflowDefinitionId,
        name: impl Into<String> + std::fmt::Debug,
        specs: SandboxSpecs,
        mode: SandboxMode,
    ) -> Result<Sandbox, SandboxError> {
        self.validate_repo_mode(&mode)?;
        let storage_name = Self::workflow_sandbox_storage_name(workflow_id, &name.into());
        let id = SandboxId::new();
        let mount_path = self.admin.mount_path(&format!("sb-{id}"));
        let new_sandbox = NewSandbox::builder()
            .id(id)
            .project_id(project_id)
            .name(storage_name)
            .specs(specs)
            .mode(mode)
            .mount_path(mount_path)
            .workflow_id(Some(workflow_id))
            .build()
            .expect("could not build new sandbox");

        let sandbox = self.repo.create_in_op(op, new_sandbox).await?;
        Audit::record_sandbox_id(sandbox.id);
        Ok(sandbox)
    }

    /// Polls until `state == Ready` or an error/terminal state. Used by
    /// the workflow executor pre-flight after create/restart.
    #[instrument(name = "domain.sandbox.wait_until_ready", skip(self))]
    pub async fn wait_until_ready(
        &self,
        id: SandboxId,
        timeout: Duration,
    ) -> Result<Sandbox, SandboxError> {
        let deadline = tokio::time::Instant::now() + timeout;
        let interval = Duration::from_millis(500);
        loop {
            let sandbox = self.repo.find_by_id(id).await?;
            match sandbox.state {
                SandboxState::Ready => return Ok(sandbox),
                SandboxState::Errored => {
                    return Err(SandboxError::NotReady {
                        state: format!(
                            "errored: {}",
                            sandbox.last_error.as_deref().unwrap_or("unknown")
                        ),
                    });
                }
                SandboxState::Suspended => {
                    return Err(SandboxError::NotReady {
                        state: "suspended".into(),
                    });
                }
                _ => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(SandboxError::NotReady {
                    state: format!("timeout in {}", sandbox.state),
                });
            }
            tokio::time::sleep(interval).await;
        }
    }

    /// `pub(crate)` helper used by the skills context builder; no auth check.
    /// `None` (unattached agent) → empty `Vec`. `Some(id)` → that one
    /// sandbox's `exported_skills` only when its state is `Ready`; other
    /// states (and unknown ids) return empty.
    #[instrument(name = "domain.sandbox.exported_skills_for_sandbox", skip(self))]
    pub(crate) async fn exported_skills_for_sandbox(
        &self,
        sandbox_id: Option<SandboxId>,
    ) -> Result<Vec<sandbox::instance_client::ExportedSkill>, SandboxError> {
        let Some(id) = sandbox_id else {
            return Ok(Vec::new());
        };
        let Some(sandbox) = self.repo.maybe_find_by_id(id).await? else {
            return Ok(Vec::new());
        };
        if sandbox.state != SandboxState::Ready {
            return Ok(Vec::new());
        }
        Ok(sandbox.exported_skills.clone())
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
            AuthResource::Sandbox(sandbox.project_id, Some(sandbox.id)),
        )?;
        Audit::record_action_if_unset("sandbox.instance_client_for");
        Audit::record_project_id(sandbox.project_id);
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

    #[instrument(name = "domain.sandbox.list_for_project", skip(self, sub))]
    pub async fn list_for_project(
        &self,
        sub: &AuthSubject,
        project_id: impl Into<ProjectId> + std::fmt::Debug,
    ) -> Result<Vec<Sandbox>, SandboxError> {
        const PAGE_SIZE: usize = 100;
        let project_id = project_id.into();
        sub.can(AuthVerb::Read, AuthResource::Sandbox(project_id, None))?;
        Audit::record_action_if_unset("sandbox.list_for_project");
        Audit::record_project_id(project_id);
        let mut all = Vec::new();
        let mut query = PaginatedQueryArgs {
            first: PAGE_SIZE,
            after: None,
        };

        loop {
            let mut result = self
                .repo
                .list_for_project_id_by_created_at(project_id, query, ListDirection::Descending)
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
            AuthResource::Sandbox(pre.project_id, Some(pre.id)),
        )?;
        Audit::record_action_if_unset("sandbox.restart");
        Audit::record_project_id(pre.project_id);
        Audit::record_sandbox_id(id);
        self.restart_inner(id).await
    }

    /// Auth-free restart used by the workflow executor pre-flight.
    #[instrument(name = "domain.sandbox.restart_for_workflow", skip(self))]
    pub async fn restart_for_workflow(&self, id: SandboxId) -> Result<Sandbox, SandboxError> {
        Audit::record_sandbox_id(id);
        self.restart_inner(id).await
    }

    async fn restart_inner(&self, id: SandboxId) -> Result<Sandbox, SandboxError> {
        let mut op = self.repo.begin_op().await?;
        let mut sandbox = self.repo.find_by_id_in_op(&mut op, id).await?;

        // Refuse to restart while the creation lifecycle is still in
        // flight — spawning a second one would race the running
        // `/initialize` (the new lifecycle's `delete_sandbox` kills the
        // original sandbox-server mid-git, leaving a stale
        // `.git/index.lock`). Caller should `create wait=true` or poll
        // `get` instead.
        match sandbox.state {
            SandboxState::Provisioning | SandboxState::Initializing => {
                return Err(SandboxError::NotReady {
                    state: format!(
                        "{} (creation in flight; use `create wait=true` or poll \
                         `get` instead of `restart`)",
                        sandbox.state
                    ),
                });
            }
            _ => {}
        }

        if sandbox.provisioning().did_execute() {
            self.repo.update_in_op(&mut op, &mut sandbox).await?;
        }
        op.commit().await?;
        self.spawn_sandbox_creation(id);
        Ok(sandbox)
    }

    /// Verifies project match (`WrongProject` else); delegates to
    /// `Sandbox::attach_agent` which enforces single-writer.
    #[instrument(
        name = "domain.sandbox.attach_to_agent_in_op",
        skip(self, op),
        fields(%project_id, %sandbox_id, %agent_id, ?mode)
    )]
    pub async fn attach_to_agent_in_op(
        &self,
        op: &mut DbOp<'_>,
        project_id: ProjectId,
        sandbox_id: SandboxId,
        agent_id: AgentId,
        mode: SandboxAgentMode,
    ) -> Result<Sandbox, SandboxError> {
        Audit::record_sandbox_id(sandbox_id);
        Audit::record_project_id(project_id);
        Audit::record_agent_id(agent_id);
        let mut sandbox = self.repo.find_by_id_in_op(&mut *op, sandbox_id).await?;
        let did_attach = sandbox
            .attach_agent(agent_id, project_id, mode)?
            .did_execute();
        if did_attach {
            self.repo.update_in_op(op, &mut sandbox).await?;
        }

        // Notify the sandbox-server that a new agent is attaching so
        // it can reset per-tenant state (today: drop the persistent
        // bash session so the next `/execute` respawns at the cwd
        // `/initialize` pinned). Best-effort — a transient 5xx is
        // logged and skipped.
        if did_attach && sandbox.state == SandboxState::Ready {
            if let Err(e) = self.notify_sandbox_attach(&sandbox).await {
                tracing::warn!(
                    sandbox_id = %sandbox_id,
                    error = %e,
                    "/attach notify failed (best-effort)"
                );
            }
        }
        Ok(sandbox)
    }

    /// POST `/attach` to the sandbox-server. Resolves `base_url` via
    /// the admin client (k8s pod IP / local port). The sandbox no
    /// longer holds a GitHub credential (memo `019dfebc` M4 — git
    /// auth flows through the drua git-proxy via the projected SA
    /// token), so the attach body is empty.
    async fn notify_sandbox_attach(&self, sandbox: &Sandbox) -> Result<(), SandboxError> {
        let view = self.admin.get_sandbox(&sandbox.resource_name()).await?;
        let Some(client) = InstanceClient::from_sandbox(&view) else {
            return Ok(());
        };
        client.attach(&AttachRequest::default()).await?;
        Ok(())
    }

    /// Best-effort hook called by the workflow executor at agent-step
    /// start: resets the sandbox repo to `origin/main` so leftover
    /// state from a prior run doesn't leak into the new agent's
    /// context. No-op for `Scratch`-mode sandboxes (the sandbox-server
    /// short-circuits when `cwd` has no `.git`). Failures (e.g. fetch
    /// blocked by a stale token) are logged at warn — the step still
    /// runs.
    #[instrument(name = "domain.sandbox.reset_for_workflow_step", skip(self))]
    pub async fn reset_for_workflow_step(&self, sandbox_id: SandboxId) {
        let sandbox = match self.repo.find_by_id(sandbox_id).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(%sandbox_id, error = %e, "reset: sandbox lookup failed");
                return;
            }
        };
        if !matches!(sandbox.mode, SandboxMode::Repo { .. }) {
            return;
        }
        let view = match self.admin.get_sandbox(&sandbox.resource_name()).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(%sandbox_id, error = %e, "reset: admin lookup failed");
                return;
            }
        };
        let Some(client) = InstanceClient::from_sandbox(&view) else {
            return;
        };
        match client.reset_repo().await {
            Ok(resp) if resp.ok => {
                tracing::info!(%sandbox_id, "reset: repo reset to clean baseline");
            }
            Ok(resp) => {
                tracing::warn!(
                    %sandbox_id,
                    output = %resp.output,
                    "reset: repo reset partially failed (continuing)"
                );
            }
            Err(e) => {
                tracing::warn!(%sandbox_id, error = %e, "reset: HTTP call failed");
            }
        }
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

    /// Project-cascade entry point: tears down every sandbox owned by
    /// `project_id` end-to-end — pod / local process via the admin
    /// client, persistent storage (PVCs / `.sandboxes/<name>/`) via
    /// `delete_pvcs`, and the DB row via the repo's
    /// `soft_without_queries` cascade. Per-sandbox admin failures are
    /// logged and skipped so a transient k8s blip can't strand the
    /// whole project delete; the row flip still lands so the entity
    /// stops being addressable.
    #[instrument(
        name = "domain.sandbox.cascade_delete_for_project_in_op",
        skip(self, op)
    )]
    pub async fn cascade_delete_for_project_in_op(
        &self,
        op: &mut DbOp<'_>,
        project_id: ProjectId,
    ) -> Result<(), SandboxError> {
        for sandbox in self.list_all_for_project_in_op(op, project_id).await? {
            let resource_name = sandbox.resource_name();
            if sandbox.state != SandboxState::Suspended {
                if let Err(e) = self.admin.delete_sandbox(&resource_name).await {
                    tracing::warn!(
                        sandbox_id = %sandbox.id,
                        sandbox = %resource_name,
                        error = %e,
                        "admin delete_sandbox failed during project cascade"
                    );
                }
            }
            if let Err(e) = self.admin.delete_pvcs(&resource_name).await {
                tracing::warn!(
                    sandbox_id = %sandbox.id,
                    sandbox = %resource_name,
                    error = %e,
                    "admin delete_pvcs failed during project cascade"
                );
            }
        }
        self.repo
            .cascade_delete_for_project_in_op(op, project_id)
            .await?;
        Ok(())
    }

    #[instrument(
        name = "domain.sandbox.cascade_delete_for_workflow_id_in_op",
        skip(self, op)
    )]
    pub async fn cascade_delete_for_workflow_id_in_op(
        &self,
        op: &mut DbOp<'_>,
        workflow_id: WorkflowDefinitionId,
    ) -> Result<(), SandboxError> {
        for sandbox in self.list_all_for_workflow_id_in_op(op, workflow_id).await? {
            let resource_name = sandbox.resource_name();
            if sandbox.state != SandboxState::Suspended {
                if let Err(e) = self.admin.delete_sandbox(&resource_name).await {
                    tracing::warn!(
                        sandbox_id = %sandbox.id,
                        sandbox = %resource_name,
                        error = %e,
                        "admin delete_sandbox failed during workflow cascade"
                    );
                }
            }
            if let Err(e) = self.admin.delete_pvcs(&resource_name).await {
                tracing::warn!(
                    sandbox_id = %sandbox.id,
                    sandbox = %resource_name,
                    error = %e,
                    "admin delete_pvcs failed during workflow cascade"
                );
            }
        }
        self.repo
            .cascade_delete_for_workflow_id_in_op(op, workflow_id)
            .await?;
        Ok(())
    }

    async fn list_all_for_workflow_id_in_op(
        &self,
        op: &mut DbOp<'_>,
        workflow_id: WorkflowDefinitionId,
    ) -> Result<Vec<Sandbox>, SandboxError> {
        const PAGE_SIZE: usize = 100;
        let mut all = Vec::new();
        let mut query = PaginatedQueryArgs {
            first: PAGE_SIZE,
            after: None,
        };
        loop {
            let mut result = self
                .repo
                .list_for_workflow_id_by_created_at_in_op(
                    &mut *op,
                    Some(workflow_id),
                    query,
                    ListDirection::Descending,
                )
                .await?;
            all.append(&mut result.entities);
            match result.into_next_query() {
                Some(next) => query = next,
                None => break,
            }
        }
        Ok(all)
    }

    /// Drains every page of `list_for_project_id_by_created_at_in_op`
    /// — used by the cascade variants that *must not* miss rows.
    async fn list_all_for_project_in_op(
        &self,
        op: &mut DbOp<'_>,
        project_id: ProjectId,
    ) -> Result<Vec<Sandbox>, SandboxError> {
        const PAGE_SIZE: usize = 100;
        let mut all = Vec::new();
        let mut query = PaginatedQueryArgs {
            first: PAGE_SIZE,
            after: None,
        };
        loop {
            let mut result = self
                .repo
                .list_for_project_id_by_created_at_in_op(
                    &mut *op,
                    project_id,
                    query,
                    ListDirection::Descending,
                )
                .await?;
            all.append(&mut result.entities);
            match result.into_next_query() {
                Some(next) => query = next,
                None => break,
            }
        }
        Ok(all)
    }

    /// Re-allocate CPU / memory / disk for a single sandbox. Pod-level
    /// fields (cpu, memory) only take effect on the next `restart`;
    /// disk growth is applied immediately to the backing PVC. Disk
    /// shrinks are rejected — k8s storage providers don't support it
    /// and the local backend doesn't enforce sizes anyway. Callers
    /// pass only the fields they want changed; unset fields are
    /// preserved as-of the in-transaction load so a concurrent resize
    /// can't make a CPU-only request look like a disk shrink.
    #[instrument(name = "domain.sandbox.resize", skip(self, sub))]
    pub async fn resize(
        &self,
        sub: &AuthSubject,
        id: impl Into<SandboxId> + std::fmt::Debug,
        request: ResizeRequest,
    ) -> Result<Sandbox, SandboxError> {
        let id = id.into();
        let sandbox = self.repo.find_by_id(id).await?;
        sub.can(
            AuthVerb::Update,
            AuthResource::Sandbox(sandbox.project_id, Some(sandbox.id)),
        )?;
        Audit::record_action_if_unset("sandbox.resize");
        Audit::record_project_id(sandbox.project_id);
        let mut op = self.repo.begin_op().await?;
        let sandbox = self.resize_in_op(&mut op, id, request).await?;
        op.commit().await?;
        Ok(sandbox)
    }

    /// Reads the sandbox in-transaction, merges `request` with the
    /// current specs (so unset fields are filled from the in-tx row,
    /// never a pre-tx read that a concurrent resize could have
    /// invalidated), then validates / applies. PVC growth is
    /// best-effort: a failure logs a warning but still records the
    /// new entity-level allocation, mirroring the admin-delete branch
    /// in `suspend_in_op`. Callers should `restart` to apply
    /// CPU/memory changes.
    #[instrument(name = "domain.sandbox.resize_in_op", skip(self, op))]
    pub async fn resize_in_op(
        &self,
        op: &mut DbOp<'_>,
        id: impl Into<SandboxId> + std::fmt::Debug,
        request: ResizeRequest,
    ) -> Result<Sandbox, SandboxError> {
        let id = id.into();
        Audit::record_sandbox_id(id);
        let mut sandbox = self.repo.find_by_id_in_op(&mut *op, id).await?;
        let specs = request.merge_with(&sandbox.specs);

        if !disk_size_grows(&sandbox.specs.disk_size, &specs.disk_size) {
            return Err(SandboxError::DiskShrinkNotSupported {
                current: sandbox.specs.disk_size.clone(),
                requested: specs.disk_size.clone(),
            });
        }

        if !sandbox.resize(specs.clone()).did_execute() {
            return Ok(sandbox);
        }
        self.repo.update_in_op(op, &mut sandbox).await?;

        let resource_name = sandbox.resource_name();
        if let Err(e) = self
            .admin
            .resize_pvc(&resource_name, &specs.disk_size)
            .await
        {
            tracing::warn!(
                sandbox = %resource_name,
                error = %e,
                "Admin client resize_pvc failed; entity specs still updated — \
                 restart will recreate the pod with new CPU/memory"
            );
        }
        Ok(sandbox)
    }

    /// Per-sandbox delete: tears down pod + PVCs via the admin client,
    /// pushes a `Deleted` event, and soft-deletes the row. Unlike
    /// [`Self::cascade_delete_for_project_in_op`], this path is the
    /// auditable single-sandbox tombstone.
    #[instrument(name = "domain.sandbox.delete", skip(self, sub))]
    pub async fn delete(
        &self,
        sub: &AuthSubject,
        id: impl Into<SandboxId> + std::fmt::Debug,
    ) -> Result<(), SandboxError> {
        let id = id.into();
        let sandbox = self.repo.find_by_id(id).await?;
        sub.can(
            AuthVerb::Delete,
            AuthResource::Sandbox(sandbox.project_id, Some(sandbox.id)),
        )?;
        Audit::record_action_if_unset("sandbox.delete");
        Audit::record_project_id(sandbox.project_id);
        let mut op = self.repo.begin_op().await?;
        self.delete_in_op(&mut op, id).await?;
        op.commit().await?;
        Ok(())
    }

    /// Admin teardown is best-effort: a stuck pod or PVC can't strand
    /// the row, otherwise the entity becomes un-deletable and the
    /// project cascade later picks up a tombstone anyway.
    #[instrument(name = "domain.sandbox.delete_in_op", skip(self, op))]
    pub async fn delete_in_op(
        &self,
        op: &mut DbOp<'_>,
        id: impl Into<SandboxId> + std::fmt::Debug,
    ) -> Result<(), SandboxError> {
        let id = id.into();
        Audit::record_sandbox_id(id);
        let mut sandbox = self.repo.find_by_id_in_op(&mut *op, id).await?;

        let resource_name = sandbox.resource_name();
        if sandbox.state != SandboxState::Suspended {
            if let Err(e) = self.admin.delete_sandbox(&resource_name).await {
                match e {
                    sandbox::AdminError::NotFound(_) => {}
                    other => tracing::warn!(
                        sandbox = %resource_name,
                        error = %other,
                        "admin delete_sandbox failed during single-sandbox delete"
                    ),
                }
            }
        }
        if let Err(e) = self.admin.delete_pvcs(&resource_name).await {
            tracing::warn!(
                sandbox = %resource_name,
                error = %e,
                "admin delete_pvcs failed during single-sandbox delete"
            );
        }

        if sandbox.delete().did_execute() {
            self.repo.update_in_op(&mut *op, &mut sandbox).await?;
        }
        let to_delete = self.repo.find_by_id_in_op(&mut *op, id).await?;
        self.repo.delete_in_op(op, to_delete).await?;
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
            AuthResource::Sandbox(sandbox.project_id, Some(sandbox.id)),
        )?;
        Audit::record_action_if_unset("sandbox.suspend");
        Audit::record_project_id(sandbox.project_id);
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

/// `true` when `new` is larger than or equal to `current`. Falls back
/// to string equality when either side fails to parse, so a typo
/// doesn't silently reject a no-op resize. Uses the same parser as
/// the k8s admin client's PVC patch idempotency check so both layers
/// agree on what counts as a shrink.
fn disk_size_grows(current: &str, new: &str) -> bool {
    match (parse_k8s_quantity(current), parse_k8s_quantity(new)) {
        (Some(c), Some(n)) => n >= c,
        _ => current == new,
    }
}

/// Partial spec for [`Sandboxes::resize`]. Each field is `Some` only
/// when the caller wants it changed; unset fields are filled from
/// the in-transaction sandbox row inside `resize_in_op`, so a
/// concurrent resize between the tool handler's pre-read and the
/// service's in-tx read can't make a CPU-only request look like a
/// disk shrink (Cursor bugbot review, PR #399).
#[derive(Debug, Clone, Default)]
pub struct ResizeRequest {
    pub cpu: Option<String>,
    pub memory: Option<String>,
    pub disk_size: Option<String>,
}

impl ResizeRequest {
    pub fn is_empty(&self) -> bool {
        self.cpu.is_none() && self.memory.is_none() && self.disk_size.is_none()
    }

    fn merge_with(self, current: &SandboxSpecs) -> SandboxSpecs {
        SandboxSpecs {
            cpu: self.cpu.unwrap_or_else(|| current.cpu.clone()),
            memory: self.memory.unwrap_or_else(|| current.memory.clone()),
            disk_size: self.disk_size.unwrap_or_else(|| current.disk_size.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_size_grows_accepts_growth() {
        assert!(disk_size_grows("10Gi", "20Gi"));
        assert!(disk_size_grows("1Gi", "1024Mi"));
        assert!(disk_size_grows("10Gi", "10Gi"));
    }

    #[test]
    fn disk_size_grows_rejects_shrink() {
        assert!(!disk_size_grows("20Gi", "10Gi"));
        assert!(!disk_size_grows("1Gi", "500Mi"));
    }

    #[test]
    fn disk_size_grows_unparseable_falls_back_to_string_eq() {
        assert!(disk_size_grows("weird", "weird"));
        assert!(!disk_size_grows("weird", "other"));
    }

    #[test]
    fn resize_request_merge_preserves_unset_fields() {
        let current = SandboxSpecs {
            cpu: "1".into(),
            memory: "2Gi".into(),
            disk_size: "10Gi".into(),
        };
        // CPU-only request — disk_size must come from `current`, not
        // from a (potentially stale) caller-side default. This is the
        // race the bugbot review (PR #399) flagged.
        let req = ResizeRequest {
            cpu: Some("2".into()),
            memory: None,
            disk_size: None,
        };
        let merged = req.merge_with(&current);
        assert_eq!(merged.cpu, "2");
        assert_eq!(merged.memory, "2Gi");
        assert_eq!(merged.disk_size, "10Gi");
    }

    #[test]
    fn resize_request_is_empty_only_when_all_none() {
        assert!(ResizeRequest::default().is_empty());
        let just_cpu = ResizeRequest {
            cpu: Some("2".into()),
            ..Default::default()
        };
        assert!(!just_cpu.is_empty());
    }
}
