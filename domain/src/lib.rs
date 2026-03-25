pub mod agent;
pub mod auth;
pub mod primitives;
pub mod style_agent_logs;
pub mod user;

use agent::Agents;
use style_agent_logs::StyleAgentLogs;
use user::Users;

#[derive(Clone)]
pub struct App {
    users: Users,
    agents: Agents,
    style_agent_logs: StyleAgentLogs,
}

impl App {
    pub fn new(pool: &sqlx::PgPool) -> Self {
        Self {
            users: Users::new(pool),
            agents: Agents::new(pool),
            style_agent_logs: StyleAgentLogs::new(pool),
        }
    }

    pub fn users(&self) -> &Users {
        &self.users
    }

    pub fn agents(&self) -> &Agents {
        &self.agents
    }

    pub fn style_agent_logs(&self) -> &StyleAgentLogs {
        &self.style_agent_logs
    }
}
