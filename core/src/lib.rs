pub mod agent;
pub mod auth;
pub mod code_assistant_logs;
pub mod primitives;
pub mod user;

use std::sync::Arc;

use agent::Agents;
use code_assistant_logs::CodeAssistantLogs;
use user::Users;

#[derive(Clone)]
pub struct App {
    users: Users,
    agents: Agents,
    code_assistant_logs: Arc<CodeAssistantLogs>,
}

impl App {
    pub fn new(pool: &sqlx::PgPool) -> Self {
        Self {
            users: Users::new(pool),
            agents: Agents::new(pool),
            code_assistant_logs: Arc::new(CodeAssistantLogs::new(pool)),
        }
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
}
