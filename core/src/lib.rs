pub mod agent;
pub mod audit;
pub mod auth;
pub mod code_assistant;
mod config;
pub mod encryption;
pub mod github_app;
pub mod library;
pub mod mcp_creds;
pub mod note;
pub mod primitives;
pub mod prompt_executor;
pub mod sandbox;
pub mod skill;
pub mod toolset;
pub mod tunnel;
pub mod user;
pub mod workflow;
pub mod workspace;
pub mod workspace_secret;

pub use config::*;

use std::sync::Arc;

use agent::Agents;
use audit::Audit;
use code_assistant::CodeAssistant;
use github_app::GitHubAppTokenProvider;
use library::Library;
use mcp_creds::McpCredentials;
use note::Notes;
use primitives::ContextGeneration;
use prompt_executor::PromptExecutor;
use sandbox::Sandboxes;
use skill::Skills;
use toolset::{
    AdminToolSet, Bash, CodeAssistantToolSet, GlobTool, Grep, Ls, NotesTool, Read, SkillTool,
    TextEditor, ToolSets, ToolSetsError, UseSkillTool, WorkflowTool, WorkspaceAgent, WorkspaceLog,
    WorkspaceSandbox,
};
use user::Users;
use workflow::Workflows;
use workspace::Workspaces;
use workspace_secret::WorkspaceSecrets;

#[derive(Clone)]
pub struct App {
    users: Arc<Users>,
    mcp_creds: Arc<McpCredentials>,
    agents: Arc<Agents>,
    audit: Arc<Audit>,
    code_assistant: Option<Arc<CodeAssistant>>,
    toolsets: Arc<ToolSets>,
    workspaces: Arc<Workspaces>,
    workspace_secrets: Arc<WorkspaceSecrets>,
    skills: Arc<Skills>,
    sandboxes: Arc<Sandboxes>,
    workflows: Arc<Workflows>,
    github_app: Option<Arc<GitHubAppTokenProvider>>,
    /// Keyed by `deployment_id`; `/tunnel/ws` evicts a previous tunnel when
    /// a new connector registers the same `deployment_id`.
    tunnels: Arc<tunnel::TunnelRegistry>,
    library: Library,
    notes: Arc<Notes>,
    jobs: Arc<job::Jobs>,
    /// Held so the executor's worker task lives as long as `App`.
    _prompt_executor: Arc<PromptExecutor>,
}

impl App {
    pub async fn init(pool: &sqlx::PgPool, config: AppConfig) -> Result<Self, AppError> {
        // Fail loudly at startup rather than on first workspace-create.
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

        let audit = Arc::new(Audit::new(pool));
        let mut toolsets = ToolSets::init(config.toolsets).await?;
        toolsets.log_init_summary();
        if let Some(ca) = code_assistant.as_ref() {
            toolsets.register_searchable(CodeAssistantToolSet::new(Arc::clone(ca)));
        }
        toolsets.register_top_level(WorkspaceLog::new(Arc::clone(&audit)));
        toolsets.set_audit(Arc::clone(&audit));

        let (prompt_executor, prompt_tx) = PromptExecutor::init(config.prompt_executor).await;
        let prompt_executor = Arc::new(prompt_executor);

        let mcp_creds = McpCredentials::new(pool);

        let encryption_key = config.encryption.encryption_key();
        let workspace_secrets = WorkspaceSecrets::new(pool, encryption_key);

        // Built before Sandboxes::init so the provider can mint a fresh
        // installation token for `/initialize` to clone private repos.
        // Verify by generating a token at startup — crash on failure rather
        // than silently skip.
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

        let job_config = job::JobSvcConfig::builder()
            .pool(pool.clone())
            .build()
            .expect("Failed to build JobSvcConfig");
        let mut jobs = job::Jobs::init(job_config)
            .await
            .map_err(|e| AppError::Job(e.to_string()))?;
        let library = Library::init(
            &config.library,
            pool,
            embedder.clone(),
            &mut jobs,
            github_app.clone(),
        )
        .await
        .map_err(|e| AppError::Library(e.to_string()))?;

        // Bumped by Notes/Skills mutations (local) and PG NOTIFY (peers).
        // Read on the hot path of `Agents::send_message` to skip DB
        // round-trips when nothing changed.
        let context_generation = ContextGeneration::new();
        spawn_context_generation_listener(pool.clone(), context_generation.clone());

        let sandboxes = Arc::new(Sandboxes::init(pool, config.sandbox, github_app.clone()).await?);
        let skills = Arc::new(Skills::new(
            pool,
            Arc::clone(&sandboxes),
            library.clone(),
            context_generation.clone(),
        ));

        // Sandbox-backed tools must be registered after sandboxes is up
        // but before toolsets is wrapped in Arc.
        toolsets.register_top_level(Bash::new(Arc::clone(&sandboxes)));
        toolsets.register_top_level(TextEditor::new(Arc::clone(&sandboxes)));
        toolsets.register_top_level(Grep::new(Arc::clone(&sandboxes)));
        toolsets.register_top_level(GlobTool::new(Arc::clone(&sandboxes)));
        toolsets.register_top_level(Read::new(Arc::clone(&sandboxes)));
        toolsets.register_top_level(Ls::new(Arc::clone(&sandboxes)));
        let toolsets = Arc::new(toolsets);

        // Created before Agents so pinned notes can be injected into agent
        // system prompts at creation time.
        let notes = Arc::new(Notes::new(
            pool,
            library.clone(),
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
        ));

        toolsets.register_top_level(WorkspaceAgent::new(
            Arc::clone(&agents),
            Arc::clone(&sandboxes),
        ));
        toolsets.register_top_level(WorkspaceSandbox::new(Arc::clone(&sandboxes)));

        // Register the workflow `execute-run` job initializer with the
        // job system and obtain the spawner so `Workflows` can enqueue
        // background runs that survive deploys.
        let execute_run_initializer = Workflows::execute_run_job_initializer(
            pool,
            Arc::clone(&agents),
            Arc::clone(&skills),
            Arc::clone(&sandboxes),
        );
        let execute_run_spawner = jobs.add_initializer(execute_run_initializer);

        let workflows = Arc::new(Workflows::new(
            pool,
            library.clone(),
            Arc::clone(&skills),
            Arc::clone(&sandboxes),
            execute_run_spawner,
        ));

        let workspaces = Arc::new(Workspaces::new(
            pool,
            Arc::clone(&agents),
            Arc::clone(&sandboxes),
            Arc::clone(&skills),
            Arc::clone(&notes),
            workspace_secrets.clone(),
            Arc::clone(&workflows),
            library.clone(),
        ));
        toolsets.register_top_level(NotesTool::new(Arc::clone(&notes), Arc::clone(&workspaces)));
        toolsets.register_top_level(UseSkillTool::new(Arc::clone(&skills)));
        toolsets.register_top_level(SkillTool::new(Arc::clone(&skills), Arc::clone(&workspaces)));
        toolsets.register_top_level(WorkflowTool::new(
            Arc::clone(&workflows),
            Arc::clone(&workspaces),
            None,
        ));

        // Behind progressive disclosure to keep top-level list_tools small.
        toolsets.register_searchable(AdminToolSet::new(
            Arc::clone(&agents),
            Arc::clone(&sandboxes),
            Arc::clone(&audit),
            Arc::clone(&workspaces),
        ));

        // Reverse-sync: poll library repo for skill changes and upsert into
        // DB. Uses library-lock queue to serialize with forward-sync jobs.
        {
            use skill::job::{SyncSkillsFromLibraryConfig, SyncSkillsFromLibraryJobInitializer};
            let sync_init = SyncSkillsFromLibraryJobInitializer::new(
                library.clone(),
                Arc::clone(&skills),
                Arc::clone(&workspaces),
            );
            let sync_spawner = jobs.add_initializer(sync_init);
            sync_spawner
                .spawn_with_queue_id(
                    job::JobId::new(),
                    SyncSkillsFromLibraryConfig {
                        sync_interval_secs: config.library.skill_sync_interval_secs,
                        last_sync_commit: None,
                    },
                    library::LIBRARY_LOCK_QUEUE,
                )
                .await
                .map_err(|e| AppError::Job(e.to_string()))?;
        }

        // Reverse-sync for workflow YAML files. Same shape as the
        // skill sync above — interval-poll the library, parse +
        // upsert, share the library-lock queue.
        {
            let sync_init = workflow::SyncWorkflowsFromLibraryJobInitializer::new(
                library.clone(),
                Arc::clone(&workflows),
                Arc::clone(&workspaces),
            );
            let sync_spawner = jobs.add_initializer(sync_init);
            sync_spawner
                .spawn_with_queue_id(
                    job::JobId::new(),
                    workflow::SyncWorkflowsFromLibraryConfig {
                        sync_interval_secs: config.library.skill_sync_interval_secs,
                        last_sync_commit: None,
                    },
                    library::LIBRARY_LOCK_QUEUE,
                )
                .await
                .map_err(|e| AppError::Job(e.to_string()))?;
        }

        jobs.start_poll()
            .await
            .map_err(|e| AppError::Job(e.to_string()))?;
        let jobs = Arc::new(jobs);

        Ok(Self {
            users: Arc::new(Users::new(pool)),
            mcp_creds: Arc::new(mcp_creds),
            agents: Arc::clone(&agents),
            audit,
            code_assistant,
            toolsets,
            workspaces,
            workspace_secrets: Arc::new(workspace_secrets),
            skills,
            sandboxes,
            workflows,
            github_app,
            tunnels: Arc::new(tunnel::TunnelRegistry::new()),
            library,
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

    pub fn workspaces(&self) -> &Workspaces {
        &self.workspaces
    }

    pub fn workspace_secrets(&self) -> &WorkspaceSecrets {
        &self.workspace_secrets
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

    pub fn tunnels(&self) -> &tunnel::TunnelRegistry {
        &self.tunnels
    }

    pub fn library(&self) -> &Library {
        &self.library
    }

    pub fn notes(&self) -> &Notes {
        &self.notes
    }

    /// Gracefully shut down background jobs. Call on SIGTERM / ctrl-c.
    pub async fn shutdown(&self) {
        if let Err(e) = self.jobs.shutdown().await {
            tracing::error!(error = %e, "job shutdown failed");
        }
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
}
