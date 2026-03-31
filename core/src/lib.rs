pub mod agent;
pub mod audit;
pub mod auth;
pub mod code_assistant;
mod config;
pub mod primitives;
pub mod toolset;
pub mod user;

pub use config::*;

use std::sync::Arc;

use agent::Agents;
use audit::Audit;
use code_assistant::CodeAssistant;
use toolset::{ToolSets, ToolSetsError};
use user::Users;

#[derive(Clone)]
pub struct App {
    users: Users,
    agents: Agents,
    audit: Arc<Audit>,
    code_assistant: Option<CodeAssistant>,
    toolsets: Arc<ToolSets>,
}

impl App {
    pub async fn init(pool: &sqlx::PgPool, config: AppConfig) -> Result<Self, AppError> {
        let code_assistant = code_assistant::init(
            pool,
            &code_assistant::CodeAssistantConfig {
                db_path: config.toolsets.code_assistant.db_path.clone(),
            },
        )
        .map_err(|e| AppError::CodeAssistant(e.to_string()))?;
        let audit = Arc::new(Audit::new(pool));
        let toolsets = ToolSets::init(
            config.toolsets,
            code_assistant.clone().map(Arc::new),
            Some(Arc::clone(&audit)),
        )
        .await?;
        Ok(Self {
            users: Users::new(pool),
            agents: Agents::new(pool),
            audit,
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

    pub fn audit(&self) -> &Audit {
        &self.audit
    }

    pub fn code_assistant(&self) -> Option<&CodeAssistant> {
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
