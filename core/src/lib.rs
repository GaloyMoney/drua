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
use toolset::{ToolSets, ToolSetsError};
use user::Users;

#[derive(Clone)]
pub struct App {
    users: Users,
    agents: Agents,
    code_assistant_logs: Arc<CodeAssistantLogs>,
    toolsets: Arc<ToolSets>,
}

impl App {
    pub async fn init(pool: &sqlx::PgPool, config: AppConfig) -> Result<Self, AppError> {
        let toolsets = ToolSets::init(config.toolsets).await?;
        Ok(Self {
            users: Users::new(pool),
            agents: Agents::new(pool),
            code_assistant_logs: Arc::new(CodeAssistantLogs::new(pool)),
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

    pub fn toolsets(&self) -> &ToolSets {
        &self.toolsets
    }
}

#[derive(thiserror::Error, Debug)]
pub enum AppError {
    #[error("AppError - ToolSets: {0}")]
    ToolSets(#[from] ToolSetsError),
}
