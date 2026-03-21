use crate::primitives::*;

#[derive(Debug, Clone)]
pub enum AuthContext {
    User(UserId),
    Agent(AgentId, UserId),
    Anonymous,
}

impl AuthContext {
    pub fn user_id(&self) -> async_graphql::Result<UserId> {
        match self {
            AuthContext::User(user_id) => Ok(*user_id),
            AuthContext::Agent(_, _) => {
                Err(async_graphql::Error::new("Agent auth not allowed here"))
            }
            AuthContext::Anonymous => Err(async_graphql::Error::new("Authentication required")),
        }
    }
}
