use crate::primitives::*;

/// Unified authentication context resolved from session or bearer token.
/// Shared between web and mcp-gateway crates.
#[derive(Debug, Clone)]
pub enum AuthContext {
    User(UserId),
    McpCreds(McpCredsId, UserId),
    Anonymous,
}

impl AuthContext {
    pub fn user_id(&self) -> Result<UserId, &'static str> {
        match self {
            AuthContext::User(user_id) => Ok(*user_id),
            AuthContext::McpCreds(_, _) => Err("McpCreds auth not allowed here"),
            AuthContext::Anonymous => Err("Authentication required"),
        }
    }
}
