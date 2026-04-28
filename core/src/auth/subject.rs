use super::{error::AuthorizationError, AuthResource, AuthScope, AuthVerb};
use crate::primitives::{AgentId, McpCredsId, SandboxId, UserId, UserMessageSource, WorkspaceId};

/// Authentication subject resolved from session or bearer token.
#[derive(Debug, Clone)]
pub enum AuthSubject {
    User(UserId),
    /// Bearer token issued to a user (exported agent credential).
    ExportedAgent(UserId, McpCredsId, Vec<AuthScope>),
    Agent(WorkspaceId, AgentId, Vec<AuthScope>),
    /// Agent acting on behalf of a `User` or `ExportedAgent` originator,
    /// so downstream actions are attributable back to the user.
    AgentOnBehalfOfUser(UserId, WorkspaceId, AgentId, Vec<AuthScope>),
    Anonymous,
}

impl AuthSubject {
    /// Users are implicitly permitted everything. Scoped subjects succeed
    /// if any carried [`AuthScope`] permits the action.
    pub fn can(&self, verb: AuthVerb, resource: AuthResource) -> Result<(), AuthorizationError> {
        match self {
            AuthSubject::User(_) => Ok(()),
            AuthSubject::Anonymous => Err(AuthorizationError::AuthenticationRequired),
            _ => {
                if self.scopes().iter().any(|s| s.permits(verb, &resource)) {
                    Ok(())
                } else {
                    Err(AuthorizationError::Forbidden { verb, resource })
                }
            }
        }
    }

    pub fn user_id(&self) -> Result<UserId, &'static str> {
        match self {
            AuthSubject::User(user_id) => Ok(*user_id),
            AuthSubject::ExportedAgent(_, _, _) => Err("ExportedAgent auth not allowed here"),
            AuthSubject::Agent(_, _, _) => Err("Agent auth not allowed here"),
            AuthSubject::AgentOnBehalfOfUser(_, _, _, _) => Err("Agent auth not allowed here"),
            AuthSubject::Anonymous => Err("Authentication required"),
        }
    }

    /// `None` for unattributed `Agent` and `Anonymous`.
    pub fn originating_user_id(&self) -> Option<UserId> {
        match self {
            AuthSubject::User(user_id) => Some(*user_id),
            AuthSubject::ExportedAgent(user_id, _, _) => Some(*user_id),
            AuthSubject::AgentOnBehalfOfUser(user_id, _, _, _) => Some(*user_id),
            AuthSubject::Agent(_, _, _) | AuthSubject::Anonymous => None,
        }
    }

    pub fn workspace_id(&self) -> Option<WorkspaceId> {
        match self {
            AuthSubject::Agent(workspace_id, _, _) => Some(*workspace_id),
            AuthSubject::AgentOnBehalfOfUser(_, workspace_id, _, _) => Some(*workspace_id),
            _ => None,
        }
    }

    pub fn acting_agent_id(&self) -> Option<AgentId> {
        match self {
            AuthSubject::Agent(_, agent_id, _) => Some(*agent_id),
            AuthSubject::AgentOnBehalfOfUser(_, _, agent_id, _) => Some(*agent_id),
            _ => None,
        }
    }

    pub fn scopes(&self) -> &[AuthScope] {
        match self {
            AuthSubject::ExportedAgent(_, _, scopes)
            | AuthSubject::Agent(_, _, scopes)
            | AuthSubject::AgentOnBehalfOfUser(_, _, _, scopes) => scopes,
            _ => &[],
        }
    }

    pub fn is_admin(&self) -> bool {
        self.has_scope(&AuthScope::Admin)
    }

    /// `WorkspaceRead`/`WorkspaceWrite` were collapsed into
    /// [`AuthScope::WorkspaceAdmin`]; this is the single workspace-level check.
    pub fn can_read_workspace(&self) -> bool {
        self.workspace_id()
            .is_some_and(|ws| self.has_scope(&AuthScope::WorkspaceAdmin(ws)))
    }

    /// Currently identical to [`Self::can_read_workspace`]; kept distinct so
    /// write-side call sites stay readable and can diverge later.
    pub fn can_write_workspace(&self) -> bool {
        self.can_read_workspace()
    }

    /// Users implicitly have all scopes.
    pub fn has_scope(&self, scope: &AuthScope) -> bool {
        match self {
            AuthSubject::User(_) => true,
            AuthSubject::ExportedAgent(_, _, scopes)
            | AuthSubject::Agent(_, _, scopes)
            | AuthSubject::AgentOnBehalfOfUser(_, _, _, scopes) => scopes.contains(scope),
            AuthSubject::Anonymous => false,
        }
    }

    pub fn is_agent(&self) -> bool {
        matches!(
            self,
            AuthSubject::Agent(_, _, _) | AuthSubject::AgentOnBehalfOfUser(_, _, _, _)
        )
    }

    /// Used by sandbox-backed tools to hide themselves from admins
    /// (admins orchestrate; they don't run inside sandboxes).
    pub fn is_workspace_admin(&self) -> bool {
        self.scopes()
            .iter()
            .any(|s| matches!(s, AuthScope::WorkspaceAdmin(_)))
    }

    /// `SandboxUse` implies read; first match wins.
    pub fn readable_sandbox_id(&self) -> Option<SandboxId> {
        self.scopes().iter().find_map(|s| match s {
            AuthScope::SandboxUse(id) | AuthScope::SandboxRead(id) => Some(*id),
            _ => None,
        })
    }

    /// Panics for `Anonymous` (callers must authenticate first).
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
