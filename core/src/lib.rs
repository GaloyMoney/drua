pub mod agent;
pub mod auth;
pub mod code_assistant_logs;
mod config;
pub mod primitives;
pub mod toolset;
pub mod user;

pub use config::*;

use std::sync::Arc;

use agent::Agents;
use code_assistant_logs::CodeAssistantLogs;
use code_assistant_server::CodeAssistantEndpoints;
use toolset::{ToolSets, ToolSetsError};
use user::Users;

#[derive(Clone)]
pub struct App {
    users: Users,
    agents: Agents,
    code_assistant_logs: Arc<CodeAssistantLogs>,
    code_assistant: Option<CodeAssistantEndpoints>,
    toolsets: Arc<ToolSets>,
}

impl App {
    pub async fn init(pool: &sqlx::PgPool, config: AppConfig) -> Result<Self, AppError> {
        let code_assistant_logs = Arc::new(CodeAssistantLogs::new(pool));
        let code_assistant = code_assistant_server::init_endpoints_with_logger(
            &code_assistant_server::CodeAssistantConfig {
                db_path: config.toolsets.code_assistant.db_path.clone(),
            },
            code_assistant_logs.clone(),
        )
        .map_err(|e| AppError::CodeAssistant(e.to_string()))?;
        let toolsets = ToolSets::init(config.toolsets, code_assistant_logs.clone()).await?;
        Ok(Self {
            users: Users::new(pool),
            agents: Agents::new(pool),
            code_assistant_logs,
            code_assistant,
            toolsets: Arc::new(toolsets),
        })
    }

    pub fn users(&self) -> &Users {
        &self.users
    }

    pub fn agents(&self) -> &Agents {
        &self.agents
    }

    pub fn code_assistant_logs(&self) -> &Arc<CodeAssistantLogs> {
        &self.code_assistant_logs
    }

    pub fn code_assistant(&self) -> Option<&CodeAssistantEndpoints> {
        self.code_assistant.as_ref()
    }

    pub fn toolsets(&self) -> &ToolSets {
        &self.toolsets
    }
}

#[derive(thiserror::Error, Debug)]
pub enum AppError {
    #[error("AppError - ToolSets: {0}")]
    ToolSets(#[from] ToolSetsError),
    #[error("AppError - CodeAssistant: {0}")]
    CodeAssistant(String),
}
