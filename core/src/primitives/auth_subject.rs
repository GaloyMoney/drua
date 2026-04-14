use super::{AgentId, AuthScope, McpCredsId, UserId, UserMessageSource, WorkspaceId};

/// Unified authentication subject resolved from session or bearer token.
#[derive(Debug, Clone)]
pub enum AuthSubject {
    /// Authenticated via session cookie (human user in browser).
    User(UserId),
    /// Authenticated via bearer token issued to a user (exported agent credential).
    ExportedAgent(UserId, McpCredsId, Vec<AuthScope>),
    /// Agent acting within its workspace without user attribution.
    Agent(WorkspaceId, AgentId, Vec<AuthScope>),
    /// Agent acting within its workspace on behalf of a known user — used
    /// when the agent's tool calls are triggered by a request that itself
    /// originated from a `User` or `ExportedAgent`. Carries enough context
    /// to attribute downstream actions back to the originating user.
    AgentOnBehalfOfUser(UserId, WorkspaceId, AgentId, Vec<AuthScope>),
    /// No authentication provided.
    Anonymous,
}

impl AuthSubject {
    pub fn user_id(&self) -> Result<UserId, &'static str> {
        match self {
            AuthSubject::User(user_id) => Ok(*user_id),
            AuthSubject::ExportedAgent(_, _, _) => Err("ExportedAgent auth not allowed here"),
            AuthSubject::Agent(_, _, _) => Err("Agent auth not allowed here"),
            AuthSubject::AgentOnBehalfOfUser(_, _, _, _) => Err("Agent auth not allowed here"),
            AuthSubject::Anonymous => Err("Authentication required"),
        }
    }

    /// Return the user that this subject ultimately acts for, if any. Covers
    /// direct `User`, `ExportedAgent`, and `AgentOnBehalfOfUser`. Returns
    /// `None` for unattributed `Agent` and `Anonymous`.
    pub fn originating_user_id(&self) -> Option<UserId> {
        match self {
            AuthSubject::User(user_id) => Some(*user_id),
            AuthSubject::ExportedAgent(user_id, _, _) => Some(*user_id),
            AuthSubject::AgentOnBehalfOfUser(user_id, _, _, _) => Some(*user_id),
            AuthSubject::Agent(_, _, _) | AuthSubject::Anonymous => None,
        }
    }

    /// Return the scopes associated with this auth subject.
    pub fn scopes(&self) -> &[AuthScope] {
        match self {
            AuthSubject::ExportedAgent(_, _, scopes)
            | AuthSubject::Agent(_, _, scopes)
            | AuthSubject::AgentOnBehalfOfUser(_, _, _, scopes) => scopes,
            _ => &[],
        }
    }

    /// Check whether this auth subject carries the given scope.
    /// Users (session-based) implicitly have all scopes.
    pub fn has_scope(&self, scope: &str) -> bool {
        match self {
            AuthSubject::User(_) => true,
            AuthSubject::ExportedAgent(_, _, scopes)
            | AuthSubject::Agent(_, _, scopes)
            | AuthSubject::AgentOnBehalfOfUser(_, _, _, scopes) => {
                scopes.iter().any(|s| s.as_str() == scope)
            }
            AuthSubject::Anonymous => false,
        }
    }

    /// Convert the subject into the principal that should be recorded as the
    /// originator of a message. Panics for `Anonymous` (callers must
    /// authenticate before sending messages).
    pub fn to_message_source(&self) -> UserMessageSource {
        match self {
            AuthSubject::User(user_id) => UserMessageSource::User { user_id: *user_id },
            AuthSubject::ExportedAgent(user_id, creds_id, _) => UserMessageSource::ExportedAgent {
                user_id: *user_id,
                creds_id: *creds_id,
            },
            AuthSubject::Agent(_, agent_id, _)
            | AuthSubject::AgentOnBehalfOfUser(_, _, agent_id, _) => UserMessageSource::Agent {
                agent_id: *agent_id,
            },
            AuthSubject::Anonymous => panic!("Anonymous subject has no message source"),
        }
    }
}
