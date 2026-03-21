pub mod token;

use crate::primitives::*;

/// Unified authentication context resolved from session or bearer token.
#[derive(Debug, Clone)]
pub enum AuthContext {
    User(UserId),
    Agent(AgentId, UserId),
    Anonymous,
}

impl AuthContext {
    pub fn user_id(&self) -> Result<UserId, &'static str> {
        match self {
            AuthContext::User(user_id) => Ok(*user_id),
            AuthContext::Agent(_, _) => Err("Agent auth not allowed here"),
            AuthContext::Anonymous => Err("Authentication required"),
        }
    }
}
