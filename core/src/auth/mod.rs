use crate::primitives::*;

/// Unified authentication context resolved from session or bearer token.
/// Shared between web and mcp-gateway crates.
#[derive(Debug, Clone)]
pub enum AuthContext {
    /// Authenticated via session cookie (human user in browser).
    User(UserId),
    /// Authenticated via bearer token issued to a user (exported agent credential).
    ExportedAgent(UserId, McpCredsId, Vec<String>),
    /// Agent acting within its workspace (SA token auth or internal light-agent dispatch).
    Agent(WorkspaceId, AgentId, Vec<String>),
    /// No authentication provided.
    Anonymous,
}

impl AuthContext {
    pub fn user_id(&self) -> Result<UserId, &'static str> {
        match self {
            AuthContext::User(user_id) => Ok(*user_id),
            AuthContext::ExportedAgent(_, _, _) => Err("ExportedAgent auth not allowed here"),
            AuthContext::Agent(_, _, _) => Err("Agent auth not allowed here"),
            AuthContext::Anonymous => Err("Authentication required"),
        }
    }

    /// Return the scopes associated with this auth context.
    pub fn scopes(&self) -> &[String] {
        match self {
            AuthContext::ExportedAgent(_, _, scopes) | AuthContext::Agent(_, _, scopes) => scopes,
            _ => &[],
        }
    }

    /// Check whether this auth context carries the given scope.
    /// Users (session-based) implicitly have all scopes.
    pub fn has_scope(&self, scope: &str) -> bool {
        match self {
            AuthContext::User(_) => true,
            AuthContext::ExportedAgent(_, _, scopes) | AuthContext::Agent(_, _, scopes) => {
                scopes.iter().any(|s| s == scope)
            }
            AuthContext::Anonymous => false,
        }
    }
}
