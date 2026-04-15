use super::{AgentId, AuthScope, McpCredsId, SandboxId, UserId, UserMessageSource, WorkspaceId};

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

    /// Return the workspace this subject is acting within, if any.
    pub fn workspace_id(&self) -> Option<WorkspaceId> {
        match self {
            AuthSubject::Agent(workspace_id, _, _) => Some(*workspace_id),
            AuthSubject::AgentOnBehalfOfUser(_, workspace_id, _, _) => Some(*workspace_id),
            _ => None,
        }
    }

    /// Return the agent that is acting, if any.
    pub fn acting_agent_id(&self) -> Option<AgentId> {
        match self {
            AuthSubject::Agent(_, agent_id, _) => Some(*agent_id),
            AuthSubject::AgentOnBehalfOfUser(_, _, agent_id, _) => Some(*agent_id),
            _ => None,
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

    /// True if the subject has the `Admin` scope.
    pub fn is_admin(&self) -> bool {
        self.has_scope(&AuthScope::Admin)
    }

    /// True if the subject is in a workspace and carries `WorkspaceRead` for it.
    pub fn can_read_workspace(&self) -> bool {
        self.workspace_id()
            .is_some_and(|ws| self.has_scope(&AuthScope::WorkspaceRead(ws)))
    }

    /// True if the subject is in a workspace and carries `WorkspaceWrite` for it.
    pub fn can_write_workspace(&self) -> bool {
        self.workspace_id()
            .is_some_and(|ws| self.has_scope(&AuthScope::WorkspaceWrite(ws)))
    }

    /// Check whether this auth subject carries the given scope.
    /// Users (session-based) implicitly have all scopes.
    pub fn has_scope(&self, scope: &AuthScope) -> bool {
        match self {
            AuthSubject::User(_) => true,
            AuthSubject::ExportedAgent(_, _, scopes)
            | AuthSubject::Agent(_, _, scopes)
            | AuthSubject::AgentOnBehalfOfUser(_, _, _, scopes) => scopes.contains(scope),
            AuthSubject::Anonymous => false,
        }
    }

    /// True if the subject carries any of the supplied scopes. Useful for
    /// checks like "Admin OR WorkspaceWrite(ws)".
    pub fn has_any(&self, scopes: &[AuthScope]) -> bool {
        scopes.iter().any(|s| self.has_scope(s))
    }

    /// True when the subject is an agent — i.e. `Agent` or
    /// `AgentOnBehalfOfUser`. Used by sandbox-backed tools to decide
    /// visibility (other subject kinds should never see sandbox tools).
    pub fn is_agent(&self) -> bool {
        matches!(
            self,
            AuthSubject::Agent(_, _, _) | AuthSubject::AgentOnBehalfOfUser(_, _, _, _)
        )
    }

    /// True when the subject carries an [`AuthScope::WorkspaceLead`] for
    /// any workspace. Used by sandbox-backed tools to hide themselves
    /// from leads (leads orchestrate; they don't run inside sandboxes).
    pub fn is_workspace_lead(&self) -> bool {
        self.scopes()
            .iter()
            .any(|s| matches!(s, AuthScope::WorkspaceLead(_)))
    }

    /// First sandbox the subject can read from. `SandboxUseAll` always
    /// implies `SandboxUseReadOnly`, so we accept either. Returns `None`
    /// when the subject has no sandbox attachment at all.
    pub fn readable_sandbox_id(&self) -> Option<SandboxId> {
        self.scopes().iter().find_map(|s| match s {
            AuthScope::SandboxUseAll(id) | AuthScope::SandboxUseReadOnly(id) => Some(*id),
            _ => None,
        })
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
