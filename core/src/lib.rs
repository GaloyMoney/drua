pub mod agent;
pub mod audit;
pub mod auth;
pub mod code_assistant;
mod config;
pub mod memory;
pub mod primitives;
pub mod toolset;
pub mod user;

pub use config::*;

use std::sync::Arc;

use agent::Agents;
use audit::Audit;
use code_assistant::CodeAssistant;
use memory::Memories;
use toolset::{ToolSets, ToolSetsError};
use user::Users;

#[derive(Clone)]
pub struct App {
    users: Users,
    agents: Agents,
    audit: Arc<Audit>,
    code_assistant: Option<Arc<CodeAssistant>>,
    memory: Option<Arc<Memories>>,
    toolsets: Arc<ToolSets>,
}

impl App {
    pub async fn init(pool: &sqlx::PgPool, config: AppConfig) -> Result<Self, AppError> {
        let ca_db_exists = {
            let p = &config.toolsets.code_assistant.db_path;
            !p.is_empty() && std::path::Path::new(p).exists()
        };
        let needs_embedder = ca_db_exists || config.toolsets.memory.enabled;

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

        let memory = match (&embedder, config.toolsets.memory.enabled) {
            (Some(emb), true) => Some(Arc::new(Memories::new(
                pool,
                emb.clone(),
                config.toolsets.memory.decay_half_life_days,
            ))),
            _ => None,
        };

        let audit = Arc::new(Audit::new(pool));
        let toolsets = ToolSets::init(
            config.toolsets,
            code_assistant.clone(),
            memory.clone(),
            Some(Arc::clone(&audit)),
        )
        .await?;
        Ok(Self {
            users: Users::new(pool),
            agents: Agents::new(pool),
            audit,
            code_assistant,
            memory,
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
        self.code_assistant.as_deref()
    }

    pub fn memory(&self) -> Option<&Memories> {
        self.memory.as_deref()
    }

    pub fn toolsets(&self) -> &ToolSets {
        &self.toolsets
    }
}

#[derive(thiserror::Error, Debug)]
pub enum AppError {
    #[error("AppError - ToolSets: {0}")]
    ToolSets(#[from] ToolSetsError),
    #[error("AppError - Embedder: {0}")]
    Embedder(String),
    #[error("AppError - CodeAssistant: {0}")]
    CodeAssistant(String),
}
