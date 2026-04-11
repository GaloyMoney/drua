pub mod agent;
pub mod audit;
pub mod auth;
pub mod chat_history;
pub mod code_assistant;
mod config;
pub mod encryption;
pub mod github_app;
pub mod mcp_creds;
pub mod primitives;
pub mod report;
pub mod session;
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
use report::Reports;
use toolset::{AdminToolSet, ToolSets, ToolSetsError};
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
    reports: Option<Arc<Reports>>,
    toolsets: Arc<ToolSets>,
    workspaces: Workspaces,
    workspace_secrets: WorkspaceSecrets,
    github_app: Option<GitHubAppTokenProvider>,
}

impl App {
    pub async fn init(pool: &sqlx::PgPool, config: AppConfig) -> Result<Self, AppError> {
        let ca_db_exists = {
            let p = &config.toolsets.code_assistant.db_path;
            !p.is_empty() && std::path::Path::new(p).exists()
        };
        let needs_embedder = ca_db_exists || config.toolsets.report.enabled;

        let embedder = if needs_embedder {
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

        let reports = match (&embedder, config.toolsets.report.enabled) {
            (Some(emb), true) => Some(Arc::new(Reports::new(pool, emb.clone()))),
            _ => None,
        };

        let audit = Arc::new(Audit::new(pool));
        let toolsets = ToolSets::init(
            config.toolsets,
            code_assistant.clone(),
            reports.clone(),
            Some(Arc::clone(&audit)),
        )
        .await?;
        let toolsets = Arc::new(toolsets);
        let mcp_creds = McpCredentials::new(pool);
        let agents = Arc::new(Agents::init(pool, config.agents, Arc::clone(&toolsets)).await?);
        let workspaces = Workspaces::new(pool, Arc::clone(&agents));

        // Register admin toolset after agents/workspaces to break circular dep
        toolsets.register(AdminToolSet::new(workspaces.clone(), Arc::clone(&agents)));

        let encryption_key = config.encryption.encryption_key();
        let workspace_secrets = WorkspaceSecrets::new(pool, encryption_key);

        // Optionally initialize GitHub App token provider from AppConfig.
        // If configured, verify it works by generating a token — crash on failure
        // so broken config is caught immediately rather than silently skipped.
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

        Ok(Self {
            users: Users::new(pool),
            mcp_creds,
            agents: Arc::clone(&agents),
            audit,
            code_assistant,
            reports,
            toolsets,
            workspaces,
            workspace_secrets,
            github_app,
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

    pub fn reports(&self) -> Option<&Reports> {
        self.reports.as_deref()
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
}
