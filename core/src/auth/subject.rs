use super::{error::AuthorizationError, AuthResource, AuthScope, AuthVerb};
use crate::primitives::{AgentId, McpCredsId, SandboxId, UserId, UserMessageSource, WorkspaceId};

/// Unified authentication subject resolved from session or bearer token.
///
/// Workspace identity for agent variants is carried inside the scopes
/// vec as [`AuthScope::WorkspaceMember`] — use [`Self::workspace_id`]
/// to extract it.
#[derive(Debug, Clone)]
pub enum AuthSubject {
    /// Authenticated via session cookie (human user in browser).
    User(UserId),
    /// Authenticated via bearer token issued to a user (exported agent credential).
    ExportedAgent(UserId, McpCredsId, Vec<AuthScope>),
    /// Agent acting within its workspace without user attribution.
    /// Workspace identity is encoded as a [`AuthScope::WorkspaceMember`]
    /// scope in the scopes vec.
    Agent(AgentId, Vec<AuthScope>),
    /// Agent acting within its workspace on behalf of a known user — used
    /// when the agent's tool calls are triggered by a request that itself
    /// originated from a `User` or `ExportedAgent`. Carries enough context
    /// to attribute downstream actions back to the originating user.
    /// Workspace identity is encoded as a [`AuthScope::WorkspaceMember`]
    /// scope in the scopes vec.
    AgentOnBehalfOfUser(UserId, AgentId, Vec<AuthScope>),
    /// No authentication provided.
    Anonymous,
}

impl AuthSubject {
    /// Check whether this subject is allowed to perform `verb` on `resource`.
    /// Users (session-based) are implicitly permitted everything. For scoped
    /// subjects the check delegates to [`AuthScope::permits`] — if any
    /// carried scope grants the action, the call succeeds.
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
            AuthSubject::Agent(_, _) => Err("Agent auth not allowed here"),
            AuthSubject::AgentOnBehalfOfUser(_, _, _) => Err("Agent auth not allowed here"),
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
            AuthSubject::AgentOnBehalfOfUser(user_id, _, _) => Some(*user_id),
            AuthSubject::Agent(_, _) | AuthSubject::Anonymous => None,
        }
    }

    /// Return the workspace this subject is acting within, if any.
    ///
    /// Derives the workspace from the subject's scopes — specifically
    /// [`AuthScope::WorkspaceMember`] or [`AuthScope::WorkspaceAdmin`].
    pub fn workspace_id(&self) -> Option<WorkspaceId> {
        self.scopes().iter().find_map(|s| match s {
            AuthScope::WorkspaceMember(ws) | AuthScope::WorkspaceAdmin(ws) => Some(*ws),
            _ => None,
        })
    }

    /// Return the agent that is acting, if any.
    pub fn acting_agent_id(&self) -> Option<AgentId> {
        match self {
            AuthSubject::Agent(agent_id, _) => Some(*agent_id),
            AuthSubject::AgentOnBehalfOfUser(_, agent_id, _) => Some(*agent_id),
            _ => None,
        }
    }

    /// Return the scopes associated with this auth subject.
    pub fn scopes(&self) -> &[AuthScope] {
        match self {
            AuthSubject::ExportedAgent(_, _, scopes)
            | AuthSubject::Agent(_, scopes)
            | AuthSubject::AgentOnBehalfOfUser(_, _, scopes) => scopes,
            _ => &[],
        }
    }

    /// True if the subject has the `Admin` scope.
    pub fn is_admin(&self) -> bool {
        self.has_scope(&AuthScope::Admin)
    }

    /// True if the subject is an admin of its workspace. Currently used
    /// as the single workspace-level permission check by every workspace
    /// management tool — `WorkspaceRead` / `WorkspaceWrite` were
    /// collapsed into [`AuthScope::WorkspaceAdmin`] for simplicity.
    pub fn can_read_workspace(&self) -> bool {
        self.workspace_id()
            .is_some_and(|ws| self.has_scope(&AuthScope::WorkspaceAdmin(ws)))
    }

    /// Identical to [`Self::can_read_workspace`] for now — both gate on
    /// the lead scope. Kept as a separate name so call sites that mean
    /// "this is a write-side check" stay readable; can diverge later if
    /// we re-introduce a finer-grained scope.
    pub fn can_write_workspace(&self) -> bool {
        self.can_read_workspace()
    }

    /// Check whether this auth subject carries the given scope.
    /// Users (session-based) implicitly have all scopes.
    pub fn has_scope(&self, scope: &AuthScope) -> bool {
        match self {
            AuthSubject::User(_) => true,
            AuthSubject::ExportedAgent(_, _, scopes)
            | AuthSubject::Agent(_, scopes)
            | AuthSubject::AgentOnBehalfOfUser(_, _, scopes) => scopes.contains(scope),
            AuthSubject::Anonymous => false,
        }
    }

    /// True when the subject is an agent — i.e. `Agent` or
    /// `AgentOnBehalfOfUser`. Used by sandbox-backed tools to decide
    /// visibility (other subject kinds should never see sandbox tools).
    pub fn is_agent(&self) -> bool {
        matches!(
            self,
            AuthSubject::Agent(_, _) | AuthSubject::AgentOnBehalfOfUser(_, _, _)
        )
    }

    /// True when the subject carries an [`AuthScope::WorkspaceAdmin`]
    /// for any workspace. Used by sandbox-backed tools to hide
    /// themselves from admins (admins orchestrate; they don't run
    /// inside sandboxes).
    pub fn is_workspace_admin(&self) -> bool {
        self.scopes()
            .iter()
            .any(|s| matches!(s, AuthScope::WorkspaceAdmin(_)))
    }

    /// First sandbox the subject can read from. `SandboxUse` always
    /// implies read capability, so we accept either. Returns `None`
    /// when the subject has no sandbox attachment at all.
    pub fn readable_sandbox_id(&self) -> Option<SandboxId> {
        self.scopes().iter().find_map(|s| match s {
            AuthScope::SandboxUse(id) | AuthScope::SandboxRead(id) => Some(*id),
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
            AuthSubject::Agent(agent_id, _) | AuthSubject::AgentOnBehalfOfUser(_, agent_id, _) => {
                UserMessageSource::Agent {
                    agent_id: *agent_id,
                }
            }
            AuthSubject::Anonymous => panic!("Anonymous subject has no message source"),
        }
    }
}
