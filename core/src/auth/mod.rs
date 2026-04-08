use crate::primitives::*;

/// Unified authentication context resolved from session or bearer token.
/// Shared between web and mcp-gateway crates.
#[derive(Debug, Clone)]
pub enum AuthContext {
    /// Authenticated via session cookie (human user in browser).
    User(UserId),
    /// Authenticated via bearer token issued to a user (exported agent credential).
    ExportedAgent(UserId, McpCredsId),
    /// Internal light-agent dispatch — the agent acts on behalf of the user.
    InternalAgent(UserId, AgentId, McpCredsId),
    /// No authentication provided.
    Anonymous,
}

impl AuthContext {
    pub fn user_id(&self) -> Result<UserId, &'static str> {
        match self {
            AuthContext::User(user_id) => Ok(*user_id),
            AuthContext::ExportedAgent(_, _) => Err("ExportedAgent auth not allowed here"),
            AuthContext::InternalAgent(_, _, _) => Err("InternalAgent auth not allowed here"),
            AuthContext::Anonymous => Err("Authentication required"),
        }
    }
}
