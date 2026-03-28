pub mod agent;
pub mod auth;
pub mod primitives;
pub mod style_agent_logs;
pub mod task;
pub mod user;

use std::sync::Arc;

use agent::Agents;
use style_agent_logs::StyleAgentLogs;
use task::Tasks;
use user::Users;

#[derive(Clone)]
pub struct App {
    users: Users,
    agents: Agents,
    tasks: Tasks,
    style_agent_logs: Arc<StyleAgentLogs>,
}

impl App {
    pub fn new(pool: &sqlx::PgPool) -> Self {
        Self {
            users: Users::new(pool),
            agents: Agents::new(pool),
            tasks: Tasks::new(pool),
            style_agent_logs: Arc::new(StyleAgentLogs::new(pool)),
        }
    }

    pub fn users(&self) -> &Users {
        &self.users
    }

    pub fn agents(&self) -> &Agents {
        &self.agents
    }

    pub fn tasks(&self) -> &Tasks {
        &self.tasks
    }

    pub fn style_agent_logs(&self) -> &Arc<StyleAgentLogs> {
        &self.style_agent_logs
    }
}
