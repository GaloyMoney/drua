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

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> WorkspaceId {
        WorkspaceId::from(uuid::Uuid::parse_str("a1a2a3a4-b1b2-c1c2-d1d2-d3d4d5d6d7d8").unwrap())
    }

    fn other_ws() -> WorkspaceId {
        WorkspaceId::from(uuid::Uuid::parse_str("b1b2b3b4-c1c2-d1d2-e1e2-e3e4e5e6e7e8").unwrap())
    }

    fn member(ws: WorkspaceId) -> AuthSubject {
        AuthSubject::Agent(ws, AgentId::new(), vec![AuthScope::WorkspaceMember(ws)])
    }

    fn admin(ws: WorkspaceId) -> AuthSubject {
        AuthSubject::Agent(ws, AgentId::new(), vec![AuthScope::WorkspaceAdmin(ws)])
    }

    #[test]
    fn user_is_omnipotent() {
        let user = AuthSubject::User(UserId::new());
        assert!(user
            .can(AuthVerb::Delete, AuthResource::Sandbox(ws(), None))
            .is_ok());
    }

    #[test]
    fn anonymous_authentication_required() {
        let err = AuthSubject::Anonymous
            .can(AuthVerb::Read, AuthResource::Workspace(None))
            .unwrap_err();
        assert!(matches!(err, AuthorizationError::AuthenticationRequired));
    }

    #[test]
    fn workspace_admin_permits_management_resources() {
        let s = admin(ws());
        for verb in [
            AuthVerb::Read,
            AuthVerb::Create,
            AuthVerb::Update,
            AuthVerb::Delete,
        ] {
            assert!(s.can(verb, AuthResource::Sandbox(ws(), None)).is_ok());
            assert!(s.can(verb, AuthResource::Agent(ws(), None)).is_ok());
            assert!(s.can(verb, AuthResource::Workflow(ws(), None)).is_ok());
            assert!(s.can(verb, AuthResource::Skill(ws(), None)).is_ok());
        }
        assert!(s.can(AuthVerb::Read, AuthResource::AuditLog(ws())).is_ok());
    }

    /// Negative case: a `WorkspaceMember` cannot manage skills (or any
    /// admin-only resource) — the privileged tool would be hidden via
    /// visibility AND rejected at service entry. This is the consolidation
    /// invariant the refactor guarantees.
    #[test]
    fn workspace_member_cannot_manage_admin_resources() {
        let s = member(ws());
        assert!(s
            .can(AuthVerb::Update, AuthResource::Skill(ws(), None))
            .is_err());
        assert!(s
            .can(AuthVerb::Create, AuthResource::Sandbox(ws(), None))
            .is_err());
        assert!(s
            .can(AuthVerb::Read, AuthResource::Agent(ws(), None))
            .is_err());
        assert!(s
            .can(AuthVerb::Read, AuthResource::Workflow(ws(), None))
            .is_err());
        assert!(s.can(AuthVerb::Read, AuthResource::AuditLog(ws())).is_err());
    }

    #[test]
    fn workspace_member_can_use_skills_and_notes() {
        let s = member(ws());
        assert!(s
            .can(AuthVerb::Use, AuthResource::Skill(ws(), None))
            .is_ok());
        assert!(s
            .can(AuthVerb::Read, AuthResource::Skill(ws(), None))
            .is_ok());
        assert!(s
            .can(AuthVerb::Create, AuthResource::Note(ws(), None))
            .is_ok());
        assert!(s
            .can(AuthVerb::Update, AuthResource::Note(ws(), None))
            .is_ok());
    }

    #[test]
    fn cross_workspace_admin_cannot_access_other_workspace() {
        let s = admin(ws());
        assert!(s
            .can(AuthVerb::Read, AuthResource::Sandbox(other_ws(), None))
            .is_err());
        assert!(s
            .can(AuthVerb::Update, AuthResource::Skill(other_ws(), None))
            .is_err());
    }
}
