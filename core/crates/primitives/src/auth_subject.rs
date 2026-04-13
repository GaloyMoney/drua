use crate::ids::*;

/// Unified authentication subject resolved from session or bearer token.
#[derive(Debug, Clone)]
pub enum AuthSubject {
    /// Authenticated via session cookie (human user in browser).
    User(UserId),
    /// Authenticated via bearer token issued to a user (exported agent credential).
    ExportedAgent(UserId, McpCredsId, Vec<String>),
    /// Agent acting within its workspace (SA token auth or internal light-agent dispatch).
    Agent(WorkspaceId, AgentId, Vec<String>),
    /// No authentication provided.
    Anonymous,
}

impl AuthSubject {
    pub fn user_id(&self) -> Result<UserId, &'static str> {
        match self {
            AuthSubject::User(user_id) => Ok(*user_id),
            AuthSubject::ExportedAgent(_, _, _) => Err("ExportedAgent auth not allowed here"),
            AuthSubject::Agent(_, _, _) => Err("Agent auth not allowed here"),
            AuthSubject::Anonymous => Err("Authentication required"),
        }
    }

    /// Return the scopes associated with this auth subject.
    pub fn scopes(&self) -> &[String] {
        match self {
            AuthSubject::ExportedAgent(_, _, scopes) | AuthSubject::Agent(_, _, scopes) => scopes,
            _ => &[],
        }
    }

    /// Check whether this auth subject carries the given scope.
    /// Users (session-based) implicitly have all scopes.
    pub fn has_scope(&self, scope: &str) -> bool {
        match self {
            AuthSubject::User(_) => true,
            AuthSubject::ExportedAgent(_, _, scopes) | AuthSubject::Agent(_, _, scopes) => {
                scopes.iter().any(|s| s == scope)
            }
            AuthSubject::Anonymous => false,
        }
    }
}
