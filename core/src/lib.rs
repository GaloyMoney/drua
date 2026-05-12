#![recursion_limit = "256"]

pub mod agent;
pub mod audit;
pub mod auth;
pub mod code_assistant;
mod config;
pub mod encryption;
pub mod git_proxy;
pub mod github_app;
pub mod library;
pub mod mcp_creds;
pub mod note;
pub mod primitives;
pub mod project;
pub mod project_secret;
pub mod prompt_executor;
pub mod sandbox;
pub mod skill;
pub mod space_fs;
pub mod toolset;
pub mod tunnel;
pub mod user;
pub mod workflow;

pub use config::*;

pub use llm::{ModelChain, ModelSpec};

use std::sync::Arc;

use agent::Agents;
use audit::Audit;
use code_assistant::CodeAssistant;
use git_proxy::GitProxies;
use github_app::GitHubAppTokenProvider;
use library::{AuthedSearch, AuthedSpaces};
use mcp_creds::McpCredentials;
use note::Notes;
use primitives::ContextGeneration;
use project::Projects;
use project_secret::ProjectSecrets;
use prompt_executor::PromptExecutor;
use sandbox::Sandboxes;
use skill::Skills;
use toolset::{
    AdminToolSet, Bash, CodeAssistantToolSet, Delete, GlobTool, Grep, LibraryToolSet, Ls, MoveFile,
    NotesTool, ProjectAgent, ProjectLog, ProjectSandbox, Read, SkillTool, SpacesTool,
    SubmitOutputTool, TextEditor, ToolSets, ToolSetsError, UseSkillTool, WorkflowTool,
};
use user::Users;
use workflow::Workflows;

#[derive(Clone)]
pub struct App {
    users: Arc<Users>,
    mcp_creds: Arc<McpCredentials>,
    agents: Arc<Agents>,
    audit: Arc<Audit>,
    code_assistant: Option<Arc<CodeAssistant>>,
    toolsets: Arc<ToolSets>,
    projects: Arc<Projects>,
    project_secrets: Arc<ProjectSecrets>,
    skills: Arc<Skills>,
    sandboxes: Arc<Sandboxes>,
    workflows: Arc<Workflows>,
    github_app: Option<Arc<GitHubAppTokenProvider>>,
    git_proxies: Arc<GitProxies>,
    /// Keyed by `deployment_id`; `/tunnel/ws` evicts a previous tunnel when
    /// a new connector registers the same `deployment_id`.
    tunnels: Arc<tunnel::TunnelRegistry>,
    tunnel_registrations: Arc<tunnel::TunnelRegistrations>,
    tunnel_runtime: TunnelRuntimeConfig,
    tunnel_http: Arc<reqwest::Client>,
    tunnel_auth: Arc<tunnel::InternalAuth>,
    library: drua_library::Library,
    spaces: Arc<AuthedSpaces>,
    search: Arc<AuthedSearch>,
    notes: Arc<Notes>,
    jobs: Arc<job::Jobs>,
    /// Held so the executor's worker task lives as long as `App`.
    _prompt_executor: Arc<PromptExecutor>,
}

impl App {
    pub async fn init(pool: &sqlx::PgPool, config: AppConfig) -> Result<Self, AppError> {
        // Fail loudly at startup rather than on first project-create.
        config.agents.validate()?;
        config
            .prompt_executor
            .validate()
            .map_err(|e| AppError::PromptExecutor(e.to_string()))?;

        let embedder = Arc::new(
            code_assistant_core::embedder::Embedder::new()
                .map_err(|e| AppError::Embedder(e.to_string()))?,
        );

        let ca_db_exists = {
            let p = &config.toolsets.code_assistant.db_path;
            !p.is_empty() && std::path::Path::new(p).exists()
        };

        let code_assistant = if ca_db_exists {
            code_assistant::init(
                pool,
                &code_assistant::CodeAssistantConfig {
                    db_path: config.toolsets.code_assistant.db_path.clone(),
                },
                embedder.clone(),
            )
            .map_err(|e| AppError::CodeAssistant(e.to_string()))?
            .map(Arc::new)
        } else {
            None
        };

        // GitHub App credentials live behind the git-proxy now —
        // sandboxes never see a token. Provider stays for the proxy's
        // upstream-fetch path. Verify by generating a token at startup
        // so a misconfigured PEM crashes boot, not first push.
        let github_app = match config.github_app {
            Some(ref gh_config) => {
                let provider = GitHubAppTokenProvider::new(gh_config)
                    .map_err(|e| AppError::GitHubApp(format!("failed to initialize: {e}")))?;
                provider
                    .generate_token()
                    .await
                    .map_err(|e| AppError::GitHubApp(format!("startup token check failed: {e}")))?;
                tracing::info!("GitHub App token provider initialized and verified");
                Some(Arc::new(provider))
            }
            None => {
                tracing::info!("GitHub App not configured — token auto-provisioning disabled");
                None
            }
        };

        let audit = Arc::new(Audit::new(pool));
        let tool_caching = Arc::new(drua_tool_caching::ToolCaching::new(
            pool,
            drua_tool_caching::ToolCachingConfig::default(),
        ));
        let toolsets = ToolSets::init(
            config.toolsets,
            Some(Arc::clone(&audit)),
            github_app.clone(),
            Some(Arc::clone(&tool_caching)),
        )
        .await?;
        toolsets.log_init_summary();
        if let Some(ca) = code_assistant.as_ref() {
            toolsets.register_searchable(CodeAssistantToolSet::new(Arc::clone(ca)));
        }
        toolsets.register_top_level(ProjectLog::new(Arc::clone(&audit)));

        let (prompt_executor, prompt_tx) = PromptExecutor::init(config.prompt_executor).await;
        let prompt_executor = Arc::new(prompt_executor);

        let mcp_creds = McpCredentials::new(pool);

        let encryption_key = config.encryption.encryption_key();
        let project_secrets = ProjectSecrets::new(pool, encryption_key);

        let job_config = job::JobSvcConfig::builder()
            .pool(pool.clone())
            .build()
            .expect("Failed to build JobSvcConfig");
        let mut jobs = job::Jobs::init(job_config)
            .await
            .map_err(|e| AppError::Job(e.to_string()))?;
        let drua_config = config.library.clone().into_drua();
        let library = drua_library::Library::init(
            pool,
            &drua_config,
            embedder.clone(),
            &mut jobs,
            github_app.clone(),
        )
        .await
        .map_err(|e| AppError::Library(e.to_string()))?;
        let users = Arc::new(Users::new(pool));
        let spaces = Arc::new(AuthedSpaces::new(
            library.spaces().clone(),
            Arc::clone(&users),
        ));
        let search = Arc::new(AuthedSearch::new(library.search().clone()));

        // Bumped by Notes/Skills mutations (local) and PG NOTIFY (peers).
        // Read on the hot path of `Agents::send_message` to skip DB
        // round-trips when nothing changed.
        let context_generation = ContextGeneration::new();
        spawn_context_generation_listener(pool.clone(), context_generation.clone());

        // Bad ref-pattern in YAML must crash boot, not silently fail every push.
        // Built before `Sandboxes::init` because both services share it —
        // `Sandboxes` pre-validates `mode: repo` against the allow-list at
        // create time so failed clones don't leak `Errored` rows + child
        // tool-server processes (PR review feedback).
        let allowlist = Arc::new(
            drua_git_proxy::Allowlist::from_config(&config.git_proxy.allowlist)
                .map_err(|e| AppError::GitProxy(format!("invalid allowlist config: {e}")))?,
        );
        tracing::info!(
            entries = allowlist.entries().len(),
            "git-proxy allow-list loaded"
        );

        let sandboxes = Arc::new(Sandboxes::init(pool, config.sandbox, allowlist.clone()).await?);

        let mirror_root = config
            .git_proxy
            .mirror_root
            .clone()
            .unwrap_or_else(|| "./.git-proxy-mirrors".to_string());
        let mirror_cfg = drua_git_proxy::MirrorConfig {
            root: std::path::PathBuf::from(&mirror_root),
            fetch_ttl: std::time::Duration::from_secs(config.git_proxy.mirror_ttl_seconds),
        };
        let mirror = drua_git_proxy::MirrorManager::new(mirror_cfg);
        let git_proxies =
            Arc::new(GitProxies::new(allowlist).with_mirror(mirror, github_app.clone()));

        // Read facade for the project↔space mount relationship. Built
        // before `Skills` so Skills can resolve mounted-space skills
        // internally (mirrors how it holds `Arc<Sandboxes>`); also
        // shared with `Agents` for `<spaces>` block + `AgentScope`.
        let space_mounts = Arc::new(library::SpaceMounts::new(
            Arc::new(project::repo::ProjectRepo::new(pool)),
            Arc::clone(&spaces),
        ));

        let skills = Arc::new(Skills::new(
            pool,
            Arc::clone(&sandboxes),
            library.clone(),
            Arc::clone(&space_mounts),
            Arc::clone(&users),
            context_generation.clone(),
        ));

        // Sandbox-backed tools must be registered after sandboxes is up
        // but before toolsets is wrapped in Arc. The five read tools
        // (text_editor, grep, glob, read, ls) are deferred — they take
        // `Arc<SpaceFs>` for the `space:<slug>/...` re-routing branch
        // and `SpaceFs` depends on Projects, which doesn't exist yet.
        toolsets.register_top_level(Bash::new(Arc::clone(&sandboxes)));
        let toolsets = Arc::new(toolsets);

        // Created before Agents so pinned notes can be injected into agent
        // system prompts at creation time.
        let notes = Arc::new(Notes::new(
            pool,
            library.clone(),
            Arc::clone(&space_mounts),
            Arc::clone(&users),
            context_generation.clone(),
        ));

        let agents = Arc::new(Agents::new(
            pool,
            config.agents,
            Arc::clone(&toolsets),
            prompt_tx,
            Arc::clone(&sandboxes),
            Arc::clone(&skills),
            Some(Arc::clone(&notes)),
            context_generation.clone(),
            Arc::clone(&space_mounts),
        ));

        toolsets.register_top_level(ProjectAgent::new(Arc::clone(&agents)));
        toolsets.register_top_level(SubmitOutputTool::new(Arc::clone(&agents)));

        let workflows = Arc::new(Workflows::init(
            pool,
            library.clone(),
            Arc::clone(&skills),
            Arc::clone(&agents),
            Arc::clone(&sandboxes),
            Arc::clone(&users),
            Arc::clone(&toolsets),
            &mut jobs,
        ));

        let projects = Arc::new(Projects::new(
            pool,
            Arc::clone(&agents),
            Arc::clone(&sandboxes),
            Arc::clone(&skills),
            Arc::clone(&notes),
            project_secrets.clone(),
            Arc::clone(&workflows),
            library.clone(),
            (*spaces).clone(),
            Arc::clone(&users),
            context_generation.clone(),
        ));

        // Sandboxless read facade for `space:<slug>/...` paths. The
        // five read tools branch on the `space:` prefix and dispatch
        // through here when present; otherwise they fall through to
        // the existing sandbox `/execute` path.
        let space_fs = Arc::new(space_fs::SpaceFs::new(
            Arc::new(library.spaces().clone()),
            Arc::clone(&projects),
            Arc::clone(&users),
        ));
        toolsets.register_top_level(TextEditor::new(
            Arc::clone(&sandboxes),
            Arc::clone(&space_fs),
        ));
        toolsets.register_top_level(Grep::new(Arc::clone(&sandboxes), Arc::clone(&space_fs)));
        toolsets.register_top_level(GlobTool::new(Arc::clone(&sandboxes), Arc::clone(&space_fs)));
        toolsets.register_top_level(Read::new(Arc::clone(&sandboxes), Arc::clone(&space_fs)));
        toolsets.register_top_level(Ls::new(Arc::clone(&sandboxes), Arc::clone(&space_fs)));
        toolsets.register_top_level(MoveFile::new(Arc::clone(&sandboxes), Arc::clone(&space_fs)));
        toolsets.register_top_level(Delete::new(Arc::clone(&sandboxes), Arc::clone(&space_fs)));

        toolsets.register_top_level(SpacesTool::new(
            Arc::clone(&spaces),
            Arc::clone(&projects),
            Arc::clone(&space_fs),
            Arc::clone(&search),
        ));
        toolsets.register_top_level(ProjectSandbox::new(Arc::clone(&sandboxes)));
        toolsets.register_top_level(NotesTool::new(Arc::clone(&notes), Arc::clone(&projects)));
        toolsets.register_top_level(UseSkillTool::new(Arc::clone(&skills)));
        toolsets.register_top_level(SkillTool::new(Arc::clone(&skills), Arc::clone(&projects)));
        toolsets.register_top_level(WorkflowTool::new(
            Arc::clone(&workflows),
            Arc::clone(&projects),
            None,
        ));

        // Read-only library lookup lives behind progressive disclosure;
        // notes/skill tools cover project-scoped writes.
        toolsets.register_searchable(LibraryToolSet::new(
            Arc::clone(&search),
            Arc::clone(&spaces),
        ));

        // Behind progressive disclosure to keep top-level list_tools small.
        toolsets.register_searchable(AdminToolSet::new(
            Arc::clone(&agents),
            Arc::clone(&sandboxes),
            Arc::clone(&audit),
            Arc::clone(&projects),
            Arc::clone(&spaces),
            Arc::clone(&space_fs),
            Arc::clone(&search),
            Arc::clone(&workflows),
            Arc::clone(&skills),
            Arc::clone(&notes),
        ));

        // Reverse-sync (file → entity) for skills + workflows runs
        // through drua_library's tick-driven importer registry. The
        // Spaces importer is wired by drua_library::Library::init;
        // SkillsImporter / WorkflowsImporter are appended here so the
        // next CommitTick routes their paths into core's services.
        library
            .register_importer(Arc::new(skill::SkillsImporter::new(
                Arc::clone(&skills),
                Arc::clone(&projects),
                (*spaces).clone(),
            )))
            .await;
        library
            .register_importer(Arc::new(workflow::WorkflowsImporter::new(
                Arc::clone(&workflows),
                Arc::clone(&projects),
            )))
            .await;
        library
            .register_importer(Arc::new(note::NotesImporter::new(
                Arc::clone(&notes),
                Arc::clone(&projects),
                (*spaces).clone(),
            )))
            .await;

        jobs.start_poll()
            .await
            .map_err(|e| AppError::Job(e.to_string()))?;
        let jobs = Arc::new(jobs);

        let tunnel_registrations = Arc::new(tunnel::TunnelRegistrations::new(pool.clone()));
        let tunnel_runtime = config.tunnel_runtime.clone();
        validate_tunnel_runtime(&tunnel_runtime)?;

        spawn_tunnel_reaper(
            Arc::clone(&tunnel_registrations),
            std::time::Duration::from_secs(tunnel_runtime.reaper_interval_secs.max(1)),
        );

        let http = Arc::new(
            reqwest::Client::builder()
                .build()
                .map_err(|e| AppError::PromptExecutor(e.to_string()))?,
        );
        let auth = Arc::new(internal_auth_from_runtime(&tunnel_runtime));
        let mut tunnel_runtime = tunnel_runtime;
        if tunnel_runtime.self_pod_addr.is_some() && matches!(*auth, tunnel::InternalAuth::Disabled)
        {
            tracing::warn!(
                "POD_IP is set but no tunnel internal_secret configured — \
                 falling back to single-replica tunnel mode. Set \
                 DRUA_TUNNEL_INTERNAL_SECRET (Helm secrets.tunnelInternalSecret) \
                 to enable cross-pod tunnel HA."
            );
            tunnel_runtime.self_pod_addr = None;
        }
        let tunnel_http = Arc::clone(&http);
        let tunnel_auth = Arc::clone(&auth);
        let tunnels = Arc::new(tunnel::TunnelRegistry::new());
        if tunnel_runtime.self_pod_addr.is_some() {
            tunnel::spawn_tunnel_registry_listener(
                pool.clone(),
                tunnel_runtime.self_pod_addr.clone(),
                Arc::clone(&toolsets),
                Arc::clone(&tunnels),
                Arc::clone(&http),
                Arc::clone(&auth),
            );
        }

        Ok(Self {
            users,
            mcp_creds: Arc::new(mcp_creds),
            agents: Arc::clone(&agents),
            audit,
            code_assistant,
            toolsets,
            projects,
            project_secrets: Arc::new(project_secrets),
            skills,
            sandboxes,
            workflows,
            github_app,
            git_proxies,
            tunnels,
            tunnel_registrations,
            tunnel_runtime,
            tunnel_http,
            tunnel_auth,
            library,
            spaces,
            search,
            notes,
            jobs,
            _prompt_executor: prompt_executor,
        })
    }

    pub fn users(&self) -> &Users {
        &self.users
    }

    pub fn mcp_creds(&self) -> &McpCredentials {
        &self.mcp_creds
    }

    pub fn agents(&self) -> &Agents {
        &self.agents
    }

    pub fn audit(&self) -> &Audit {
        &self.audit
    }

    pub fn code_assistant(&self) -> Option<&CodeAssistant> {
        self.code_assistant.as_deref()
    }

    pub fn toolsets(&self) -> &ToolSets {
        &self.toolsets
    }

    pub fn projects(&self) -> &Projects {
        &self.projects
    }

    pub fn project_secrets(&self) -> &ProjectSecrets {
        &self.project_secrets
    }

    pub fn skills(&self) -> &Skills {
        &self.skills
    }

    pub fn sandboxes(&self) -> &Sandboxes {
        &self.sandboxes
    }

    pub fn workflows(&self) -> &Workflows {
        &self.workflows
    }

    pub fn github_app(&self) -> Option<&GitHubAppTokenProvider> {
        self.github_app.as_deref()
    }

    pub fn git_proxies(&self) -> &GitProxies {
        &self.git_proxies
    }

    pub fn tunnels(&self) -> &tunnel::TunnelRegistry {
        &self.tunnels
    }

    pub fn tunnel_registrations(&self) -> &tunnel::TunnelRegistrations {
        &self.tunnel_registrations
    }

    pub fn tunnel_runtime(&self) -> &TunnelRuntimeConfig {
        &self.tunnel_runtime
    }

    pub async fn tunnel_reconcile(&self, deployment_id: &str) -> Result<(), sqlx::Error> {
        let ctx = tunnel::ReconcileCtx {
            regs: &self.tunnel_registrations,
            self_pod_addr: self.tunnel_runtime.self_pod_addr.as_deref(),
            toolsets: &self.toolsets,
            tunnels: &self.tunnels,
            http: &self.tunnel_http,
            auth: &self.tunnel_auth,
        };
        tunnel::reconcile_one_deployment(&ctx, deployment_id).await
    }

    pub fn library(&self) -> &drua_library::Library {
        &self.library
    }

    pub fn spaces(&self) -> &AuthedSpaces {
        &self.spaces
    }

    pub fn search(&self) -> &AuthedSearch {
        &self.search
    }

    pub fn notes(&self) -> &Notes {
        &self.notes
    }

    /// Gracefully shut down background jobs. Call on SIGTERM / ctrl-c.
    pub async fn shutdown(&self) {
        if let Some(addr) = self.tunnel_runtime.self_pod_addr.as_deref() {
            match self.tunnel_registrations.delete_all_owned_by(addr).await {
                Ok(n) if n > 0 => {
                    tracing::info!(count = n, owner = %addr, "tunnel rows handed off");
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "tunnel shutdown cleanup failed"),
            }
        }
        if let Err(e) = self.jobs.shutdown().await {
            tracing::error!(error = %e, "job shutdown failed");
        }
    }
}

fn spawn_tunnel_reaper(regs: Arc<tunnel::TunnelRegistrations>, interval: std::time::Duration) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.tick().await;
        loop {
            tick.tick().await;
            match regs.reap_expired().await {
                Ok(0) => {}
                Ok(n) => tracing::info!(count = n, "tunnel reaper deleted expired rows"),
                Err(e) => tracing::warn!(error = %e, "tunnel reaper failed"),
            }
        }
    });
}

fn validate_tunnel_runtime(runtime: &TunnelRuntimeConfig) -> Result<(), AppError> {
    if runtime.heartbeat_secs == 0 {
        return Err(AppError::TunnelConfig(
            "tunnel_runtime.heartbeat_secs must be > 0".into(),
        ));
    }
    if runtime.expires_after_secs < runtime.heartbeat_secs * 2 {
        return Err(AppError::TunnelConfig(format!(
            "tunnel_runtime.expires_after_secs ({}) must be >= 2x heartbeat_secs ({})",
            runtime.expires_after_secs, runtime.heartbeat_secs
        )));
    }
    if runtime.reaper_interval_secs == 0 {
        return Err(AppError::TunnelConfig(
            "tunnel_runtime.reaper_interval_secs must be > 0".into(),
        ));
    }
    Ok(())
}

fn internal_auth_from_runtime(runtime: &TunnelRuntimeConfig) -> tunnel::InternalAuth {
    match runtime.internal_secret.as_deref() {
        Some(s) if !s.is_empty() => tunnel::InternalAuth::SharedSecret {
            secret: s.to_string(),
        },
        _ => tunnel::InternalAuth::Disabled,
    }
}

#[cfg(test)]
mod ha_validation_tests {
    use super::*;

    #[test]
    fn validate_rejects_zero_heartbeat() {
        let runtime = TunnelRuntimeConfig {
            heartbeat_secs: 0,
            ..Default::default()
        };
        assert!(matches!(
            validate_tunnel_runtime(&runtime),
            Err(AppError::TunnelConfig(_))
        ));
    }

    #[test]
    fn validate_rejects_ttl_below_2x_heartbeat() {
        let runtime = TunnelRuntimeConfig {
            heartbeat_secs: 30,
            expires_after_secs: 45,
            ..Default::default()
        };
        assert!(matches!(
            validate_tunnel_runtime(&runtime),
            Err(AppError::TunnelConfig(_))
        ));
    }

    #[test]
    fn validate_accepts_default() {
        validate_tunnel_runtime(&TunnelRuntimeConfig::default()).unwrap();
    }

    #[test]
    fn validate_rejects_zero_reaper_interval() {
        let runtime = TunnelRuntimeConfig {
            reaper_interval_secs: 0,
            ..Default::default()
        };
        assert!(matches!(
            validate_tunnel_runtime(&runtime),
            Err(AppError::TunnelConfig(_))
        ));
    }

    #[test]
    fn auth_disabled_when_no_secret() {
        let runtime = TunnelRuntimeConfig {
            internal_secret: None,
            ..Default::default()
        };
        assert!(matches!(
            internal_auth_from_runtime(&runtime),
            tunnel::InternalAuth::Disabled
        ));
    }

    #[test]
    fn auth_disabled_on_empty_secret() {
        let runtime = TunnelRuntimeConfig {
            internal_secret: Some(String::new()),
            ..Default::default()
        };
        assert!(matches!(
            internal_auth_from_runtime(&runtime),
            tunnel::InternalAuth::Disabled
        ));
    }

    #[test]
    fn auth_shared_secret_when_set() {
        let runtime = TunnelRuntimeConfig {
            internal_secret: Some("hunter2".to_string()),
            ..Default::default()
        };
        assert!(matches!(
            internal_auth_from_runtime(&runtime),
            tunnel::InternalAuth::SharedSecret { .. }
        ));
    }
}

/// Listens on the `context_changed` PG channel and bumps `ContextGeneration`
/// when a peer mutates notes/skills. Local mutations bump the atomic
/// directly *and* fire `pg_notify`; self-notifications cause a harmless
/// extra bump.
fn spawn_context_generation_listener(pool: sqlx::PgPool, generation: ContextGeneration) {
    tokio::spawn(async move {
        loop {
            let mut listener = match sqlx::postgres::PgListener::connect_with(&pool).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(error = %e, "context_changed listener: connect failed; retrying");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    continue;
                }
            };
            if let Err(e) = listener.listen("context_changed").await {
                tracing::warn!(error = %e, "context_changed listener: LISTEN failed; retrying");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
            loop {
                match listener.recv().await {
                    Ok(_notification) => generation.bump(),
                    Err(e) => {
                        tracing::warn!(error = %e, "context_changed listener: recv failed; reconnecting");
                        break;
                    }
                }
            }
        }
    });
}

#[derive(thiserror::Error, Debug)]
pub enum AppError {
    #[error("AppError - Agent: {0}")]
    Agent(#[from] agent::AgentError),
    #[error("AppError - ToolSets: {0}")]
    ToolSets(#[from] ToolSetsError),
    #[error("AppError - Embedder: {0}")]
    Embedder(String),
    #[error("AppError - CodeAssistant: {0}")]
    CodeAssistant(String),
    #[error("AppError - GitHubApp: {0}")]
    GitHubApp(String),
    #[error("AppError - PromptExecutor: {0}")]
    PromptExecutor(String),
    #[error("AppError - Sandbox: {0}")]
    Sandbox(#[from] sandbox::error::SandboxError),
    #[error("AppError - Job: {0}")]
    Job(String),
    #[error("AppError - Library: {0}")]
    Library(String),
    #[error("AppError - GitProxy: {0}")]
    GitProxy(String),
    #[error("AppError - TunnelConfig: {0}")]
    TunnelConfig(String),
}
