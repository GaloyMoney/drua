pub mod agent;
pub mod auth;
pub mod primitives;
pub mod user;

use agent::Agents;
use user::Users;

#[derive(Clone)]
pub struct App {
    users: Users,
    agents: Agents,
}

impl App {
    pub fn new(pool: &sqlx::PgPool) -> Self {
        Self {
            users: Users::new(pool),
            agents: Agents::new(pool),
        }
    }

    pub fn users(&self) -> &Users {
        &self.users
    }

    pub fn agents(&self) -> &Agents {
        &self.agents
    }
}
