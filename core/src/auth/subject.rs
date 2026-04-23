use super::{error::AuthorizationError, AuthResource, AuthScope, AuthVerb};
use crate::primitives::{AgentId, McpCredsId, SandboxId, UserId, UserMessageSource, WorkspaceId};

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
    /// Authenticated via the Keybase chat bridge. Carries the Keybase
    /// username for attribution. Bypasses authz like `User`.
    Keybase(String),
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
            AuthSubject::User(_) | AuthSubject::Keybase(_) => Ok(()),
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
            AuthSubject::Keybase(_) => Err("Keybase auth not allowed here"),
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
            AuthSubject::Agent(_, _, _) | AuthSubject::Keybase(_) | AuthSubject::Anonymous => None,
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
            AuthSubject::User(_) | AuthSubject::Keybase(_) => true,
            AuthSubject::ExportedAgent(_, _, scopes)
            | AuthSubject::Agent(_, _, scopes)
            | AuthSubject::AgentOnBehalfOfUser(_, _, _, scopes) => scopes.contains(scope),
            AuthSubject::Anonymous => false,
        }
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

    /// True when the subject carries an [`AuthScope::Tunnel`] authorizing
    /// registration of a tunnel connector for `deployment_id`. Session
    /// `User` subjects bypass this check (they already have effectively
    /// all scopes); bearer-authenticated subjects must hold the exact
    /// scope — a token scoped to `galoy-staging` cannot register as
    /// `galoy-production`.
    pub fn can_register_tunnel(&self, deployment_id: &str) -> bool {
        // Short-circuit for session-authenticated humans — session auth
        // already implies full scopes (see `has_scope`). Also avoids a
        // `String` allocation on every check when the Tunnel scope is
        // unused.
        if matches!(self, AuthSubject::User(_) | AuthSubject::Keybase(_)) {
            return true;
        }
        self.scopes()
            .iter()
            .any(|s| matches!(s, AuthScope::Tunnel(id) if id == deployment_id))
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
            AuthSubject::Keybase(ref username) => UserMessageSource::Keybase {
                username: username.clone(),
            },
            AuthSubject::Anonymous => panic!("Anonymous subject has no message source"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::{AgentId, McpCredsId, UserId};

    fn a_user_id() -> UserId {
        UserId::from(uuid::Uuid::new_v4())
    }

    fn exported_agent_with(scopes: Vec<AuthScope>) -> AuthSubject {
        AuthSubject::ExportedAgent(a_user_id(), McpCredsId::from(uuid::Uuid::new_v4()), scopes)
    }

    fn agent_with(scopes: Vec<AuthScope>) -> AuthSubject {
        AuthSubject::Agent(
            WorkspaceId::from(uuid::Uuid::new_v4()),
            AgentId::from(uuid::Uuid::new_v4()),
            scopes,
        )
    }

    #[test]
    fn can_register_tunnel_matches_scoped_deployment_id() {
        let subject = exported_agent_with(vec![AuthScope::Tunnel("galoy-staging".to_owned())]);
        assert!(subject.can_register_tunnel("galoy-staging"));
    }

    /// The core auth guarantee: a token scoped to one deployment cannot
    /// register as another.
    #[test]
    fn can_register_tunnel_rejects_different_deployment_id() {
        let subject = exported_agent_with(vec![AuthScope::Tunnel("galoy-staging".to_owned())]);
        assert!(!subject.can_register_tunnel("galoy-production"));
    }

    /// A token without any Tunnel scope cannot register as anything —
    /// even for a deployment_id that happens to match an External scope string.
    #[test]
    fn can_register_tunnel_rejects_no_tunnel_scope() {
        let subject = exported_agent_with(vec![AuthScope::Admin]);
        assert!(!subject.can_register_tunnel("galoy-staging"));

        let subject_ext =
            exported_agent_with(vec![AuthScope::External("galoy-staging".to_owned())]);
        assert!(!subject_ext.can_register_tunnel("galoy-staging"));
    }

    /// Session `User` subjects bypass the check (session auth implies
    /// all scopes everywhere in the codebase).
    #[test]
    fn can_register_tunnel_user_subject_bypasses() {
        let subject = AuthSubject::User(a_user_id());
        assert!(subject.can_register_tunnel("anything"));
        assert!(subject.can_register_tunnel("galoy-production"));
    }

    /// Anonymous and scope-free Agent subjects cannot register tunnels.
    #[test]
    fn can_register_tunnel_rejects_anonymous_and_scopeless() {
        assert!(!AuthSubject::Anonymous.can_register_tunnel("galoy-staging"));
        assert!(!agent_with(vec![]).can_register_tunnel("galoy-staging"));
    }

    /// A subject can hold multiple Tunnel scopes and match any of them.
    #[test]
    fn can_register_tunnel_multiple_scopes() {
        let subject = exported_agent_with(vec![
            AuthScope::Tunnel("galoy-staging".to_owned()),
            AuthScope::Tunnel("galoy-qa".to_owned()),
        ]);
        assert!(subject.can_register_tunnel("galoy-staging"));
        assert!(subject.can_register_tunnel("galoy-qa"));
        assert!(!subject.can_register_tunnel("galoy-production"));
    }
}
