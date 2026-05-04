#![recursion_limit = "256"]

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

use std::sync::Arc;

use agent::Agents;
use audit::Audit;
use code_assistant::CodeAssistant;
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
    NotesTool, ProjectAgent, ProjectLog, ProjectSandbox, Read, SkillTool, SpacesTool, TextEditor,
    ToolSets, ToolSetsError, UseSkillTool, WorkflowTool,
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
    /// Keyed by `deployment_id`; `/tunnel/ws` evicts a previous tunnel when
    /// a new connector registers the same `deployment_id`.
    tunnels: Arc<tunnel::TunnelRegistry>,
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

        let audit = Arc::new(Audit::new(pool));
        let mut toolsets = ToolSets::init(config.toolsets).await?;
        toolsets.log_init_summary();
        if let Some(ca) = code_assistant.as_ref() {
            toolsets.register_searchable(CodeAssistantToolSet::new(Arc::clone(ca)));
        }
        toolsets.register_top_level(ProjectLog::new(Arc::clone(&audit)));
        toolsets.set_audit(Arc::clone(&audit));

        let (prompt_executor, prompt_tx) = PromptExecutor::init(config.prompt_executor).await;
        let prompt_executor = Arc::new(prompt_executor);

        let mcp_creds = McpCredentials::new(pool);

        let encryption_key = config.encryption.encryption_key();
        let project_secrets = ProjectSecrets::new(pool, encryption_key);

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

        let sandboxes = Arc::new(Sandboxes::init(pool, config.sandbox, github_app.clone()).await?);
        let skills = Arc::new(Skills::new(
            pool,
            Arc::clone(&sandboxes),
            library.clone(),
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
            Arc::clone(&users),
            context_generation.clone(),
        ));

        // Read facade for the project↔space mount relationship. Holds its
        // own `ProjectRepo` + `AuthedSpaces`; no upward dep on `Projects`,
        // so `Agents` can derive `AgentScope` without the old
        // `set_projects` cycle workaround.
        let space_mounts = library::SpaceMounts::new(
            project::repo::ProjectRepo::new(pool),
            (*spaces).clone(),
        );

        let agents = Arc::new(Agents::new(
            pool,
            config.agents,
            Arc::clone(&toolsets),
            prompt_tx,
            Arc::clone(&sandboxes),
            Arc::clone(&skills),
            Some(Arc::clone(&notes)),
            context_generation.clone(),
            space_mounts,
        ));

        toolsets.register_top_level(ProjectAgent::new(Arc::clone(&agents)));

        let workflows = Arc::new(Workflows::init(
            pool,
            library.clone(),
            Arc::clone(&skills),
            Arc::clone(&agents),
            Arc::clone(&sandboxes),
            Arc::clone(&users),
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
        toolsets.register_searchable(LibraryToolSet::new(Arc::clone(&search)));

        // Behind progressive disclosure to keep top-level list_tools small.
        toolsets.register_searchable(AdminToolSet::new(
            Arc::clone(&agents),
            Arc::clone(&sandboxes),
            Arc::clone(&audit),
            Arc::clone(&projects),
            Arc::clone(&spaces),
            Arc::clone(&space_fs),
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

        jobs.start_poll()
            .await
            .map_err(|e| AppError::Job(e.to_string()))?;
        let jobs = Arc::new(jobs);

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
            tunnels: Arc::new(tunnel::TunnelRegistry::new()),
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

    pub fn tunnels(&self) -> &tunnel::TunnelRegistry {
        &self.tunnels
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
