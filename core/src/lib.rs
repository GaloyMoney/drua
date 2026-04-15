pub mod agent;
pub mod audit;
pub mod auth;
pub mod code_assistant;
mod config;
pub mod encryption;
pub mod github_app;
pub mod mcp_creds;
pub mod primitives;
pub mod prompt_executor;
pub mod sandbox;
pub mod skill;
pub mod slash_command;
pub mod toolset;
pub mod user;
pub mod workspace;
pub mod workspace_secret;

pub use config::*;

use std::sync::Arc;

use agent::Agents;
use audit::Audit;
use code_assistant::CodeAssistant;
use github_app::GitHubAppTokenProvider;
use mcp_creds::McpCredentials;
use prompt_executor::PromptExecutor;
use sandbox::Sandboxes;
use skill::Skills;
use slash_command::SlashCommands;
use toolset::{
    AllLogs, Bash, CodeAssistantToolSet, TextEditor, ToolSets, ToolSetsError, WorkspaceLog,
};
use user::Users;
use workspace::Workspaces;
use workspace_secret::WorkspaceSecrets;

#[derive(Clone)]
pub struct App {
    users: Users,
    mcp_creds: McpCredentials,
    agents: Arc<Agents>,
    audit: Arc<Audit>,
    code_assistant: Option<Arc<CodeAssistant>>,
    toolsets: Arc<ToolSets>,
    slash_commands: Arc<SlashCommands>,
    workspaces: Workspaces,
    workspace_secrets: WorkspaceSecrets,
    skills: Skills,
    sandboxes: Sandboxes,
    github_app: Option<GitHubAppTokenProvider>,
    /// Held so the executor's worker task stays alive for the lifetime of
    /// `App`; dropped on shutdown which aborts the task.
    _prompt_executor: Arc<PromptExecutor>,
}

impl App {
    pub async fn init(pool: &sqlx::PgPool, config: AppConfig) -> Result<Self, AppError> {
        // Fail loudly at startup if `agents.builtin_roles` is missing a
        // required role, instead of erroring out on the first
        // workspace-create.
        config.agents.validate()?;
        // Same for the executor — catch empty `ANTHROPIC_API_KEY` etc.
        // before the first agent message lands.
        config
            .prompt_executor
            .validate()
            .map_err(|e| AppError::PromptExecutor(e.to_string()))?;

        let ca_db_exists = {
            let p = &config.toolsets.code_assistant.db_path;
            !p.is_empty() && std::path::Path::new(p).exists()
        };

        let embedder = if ca_db_exists {
            Some(Arc::new(
                code_assistant_core::embedder::Embedder::new()
                    .map_err(|e| AppError::Embedder(e.to_string()))?,
            ))
        } else {
            None
        };

        let code_assistant = if let Some(ref emb) = embedder {
            code_assistant::init(
                pool,
                &code_assistant::CodeAssistantConfig {
                    db_path: config.toolsets.code_assistant.db_path.clone(),
                },
                emb.clone(),
            )
            .map_err(|e| AppError::CodeAssistant(e.to_string()))?
            .map(Arc::new)
        } else {
            None
        };

        let audit = Arc::new(Audit::new(pool));
        let mut toolsets = ToolSets::init(config.toolsets).await?;
        if let Some(ca) = code_assistant.as_ref() {
            toolsets.register_searchable(CodeAssistantToolSet::new(Arc::clone(ca)));
        }
        toolsets.register_top_level(WorkspaceLog::new(Arc::clone(&audit)));
        toolsets.register_top_level(AllLogs::new(Arc::clone(&audit)));
        toolsets.set_audit(Arc::clone(&audit));

        // Spawn the prompt executor and hand its request channel to the
        // agents service; hold the executor so its worker task lives as
        // long as `App`.
        let (prompt_executor, prompt_tx) = PromptExecutor::init(config.prompt_executor).await;
        let prompt_executor = Arc::new(prompt_executor);

        let mcp_creds = McpCredentials::new(pool);
        let slash_commands = Arc::new(slash_command::default_registry());

        let encryption_key = config.encryption.encryption_key();
        let workspace_secrets = WorkspaceSecrets::new(pool, encryption_key);
        let skills = Skills::new(pool);

        // Optionally initialize GitHub App token provider from AppConfig.
        // If configured, verify it works by generating a token — crash on failure
        // so broken config is caught immediately rather than silently skipped.
        // Built before Sandboxes::init so the provider can be threaded into
        // the sandbox lifecycle (used to mint a fresh installation token
        // for `/initialize` so private repos can be cloned).
        let github_app = match config.github_app {
            Some(ref gh_config) => {
                let provider = GitHubAppTokenProvider::new(gh_config)
                    .map_err(|e| AppError::GitHubApp(format!("failed to initialize: {e}")))?;
                provider
                    .generate_token()
                    .await
                    .map_err(|e| AppError::GitHubApp(format!("startup token check failed: {e}")))?;
                tracing::info!("GitHub App token provider initialized and verified");
                Some(provider)
            }
            None => {
                tracing::info!("GitHub App not configured — token auto-provisioning disabled");
                None
            }
        };

        let sandboxes = Sandboxes::init(pool, config.sandbox, github_app.clone()).await?;

        // Sandbox-backed tools (Bash, TextEditor) need the sandboxes
        // service to resolve the running pod for an attached agent —
        // register them after sandboxes is up but before we wrap the
        // toolsets in Arc.
        toolsets.register_top_level(Bash::new(sandboxes.clone()));
        toolsets.register_top_level(TextEditor::new(sandboxes.clone()));
        let toolsets = Arc::new(toolsets);

        let agents = Arc::new(Agents::new(
            pool,
            config.agents,
            Arc::clone(&toolsets),
            prompt_tx,
            sandboxes.clone(),
        ));
        let workspaces = Workspaces::new(pool, Arc::clone(&agents));

        Ok(Self {
            users: Users::new(pool),
            mcp_creds,
            agents: Arc::clone(&agents),
            audit,
            code_assistant,
            toolsets,
            slash_commands,
            workspaces,
            workspace_secrets,
            skills,
            sandboxes,
            github_app,
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

    pub fn slash_commands(&self) -> &SlashCommands {
        &self.slash_commands
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

    pub fn github_app(&self) -> Option<&GitHubAppTokenProvider> {
        self.github_app.as_ref()
    }
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
}
