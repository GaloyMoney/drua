use super::{error::AuthorizationError, AuthResource, AuthScope, AuthVerb};
use crate::primitives::{AgentId, McpCredsId, ProjectId, SandboxId, UserId, UserMessageSource};

/// Authentication subject resolved from session or bearer token.
#[derive(Debug, Clone)]
pub enum AuthSubject {
    User(UserId),
    /// Bearer token issued to a user (exported agent credential).
    ExportedAgent(UserId, McpCredsId, Vec<AuthScope>),
    Agent(ProjectId, AgentId, Vec<AuthScope>),
    /// Agent acting on behalf of a `User` or `ExportedAgent` originator,
    /// so downstream actions are attributable back to the user.
    AgentOnBehalfOfUser(UserId, ProjectId, AgentId, Vec<AuthScope>),
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

    pub fn project_id(&self) -> Option<ProjectId> {
        match self {
            AuthSubject::Agent(project_id, _, _) => Some(*project_id),
            AuthSubject::AgentOnBehalfOfUser(_, project_id, _, _) => Some(*project_id),
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
    pub fn is_project_admin(&self) -> bool {
        self.scopes()
            .iter()
            .any(|s| matches!(s, AuthScope::ProjectAdmin(_)))
    }

    /// Visibility predicate for sandbox/space file tools (Bash, Read,
    /// LS, Glob, Grep, Edit, Move, Delete). Mirrors the dual gate
    /// these tools share: subject must be an Agent (users and
    /// anonymous never run files) and must NOT be a project admin
    /// (admins orchestrate; they don't run files themselves).
    ///
    /// Centralised here so a future scope/resource refactor can
    /// replace the body with a `can(...)` call once the auth model
    /// gains a verb that maps cleanly to "agent task tools" — see
    /// `core/src/auth/scope.rs` for the current scope model.
    pub fn can_use_agent_file_tools(&self) -> bool {
        self.is_agent() && !self.is_project_admin()
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

    fn project() -> ProjectId {
        ProjectId::from(uuid::Uuid::parse_str("a1a2a3a4-b1b2-c1c2-d1d2-d3d4d5d6d7d8").unwrap())
    }

    fn other_ws() -> ProjectId {
        ProjectId::from(uuid::Uuid::parse_str("b1b2b3b4-c1c2-d1d2-e1e2-e3e4e5e6e7e8").unwrap())
    }

    fn member(project: ProjectId) -> AuthSubject {
        AuthSubject::Agent(
            project,
            AgentId::new(),
            vec![AuthScope::ProjectMember(project)],
        )
    }

    fn admin(project: ProjectId) -> AuthSubject {
        AuthSubject::Agent(
            project,
            AgentId::new(),
            vec![AuthScope::ProjectAdmin(project)],
        )
    }

    #[test]
    fn user_is_omnipotent() {
        let user = AuthSubject::User(UserId::new());
        assert!(user
            .can(AuthVerb::Delete, AuthResource::Sandbox(project(), None))
            .is_ok());
    }

    #[test]
    fn anonymous_authentication_required() {
        let err = AuthSubject::Anonymous
            .can(AuthVerb::Read, AuthResource::Project(None))
            .unwrap_err();
        assert!(matches!(err, AuthorizationError::AuthenticationRequired));
    }

    #[test]
    fn project_admin_permits_management_resources() {
        let s = admin(project());
        for verb in [
            AuthVerb::Read,
            AuthVerb::Create,
            AuthVerb::Update,
            AuthVerb::Delete,
        ] {
            assert!(s.can(verb, AuthResource::Sandbox(project(), None)).is_ok());
            assert!(s.can(verb, AuthResource::Agent(project(), None)).is_ok());
            assert!(s.can(verb, AuthResource::Workflow(project(), None)).is_ok());
            assert!(s.can(verb, AuthResource::Skill(project(), None)).is_ok());
        }
        assert!(s
            .can(AuthVerb::Read, AuthResource::AuditLog(project()))
            .is_ok());
    }

    /// Negative case: a `ProjectMember` cannot manage skills (or any
    /// admin-only resource) — the privileged tool would be hidden via
    /// visibility AND rejected at service entry. This is the consolidation
    /// invariant the refactor guarantees.
    #[test]
    fn project_member_cannot_manage_admin_resources() {
        let s = member(project());
        assert!(s
            .can(AuthVerb::Update, AuthResource::Skill(project(), None))
            .is_err());
        assert!(s
            .can(AuthVerb::Create, AuthResource::Sandbox(project(), None))
            .is_err());
        assert!(s
            .can(AuthVerb::Read, AuthResource::Agent(project(), None))
            .is_err());
        assert!(s
            .can(AuthVerb::Read, AuthResource::Workflow(project(), None))
            .is_err());
        assert!(s
            .can(AuthVerb::Read, AuthResource::AuditLog(project()))
            .is_err());
    }

    #[test]
    fn project_member_can_use_skills_and_notes() {
        let s = member(project());
        assert!(s
            .can(AuthVerb::Use, AuthResource::Skill(project(), None))
            .is_ok());
        assert!(s
            .can(AuthVerb::Read, AuthResource::Skill(project(), None))
            .is_ok());
        assert!(s
            .can(AuthVerb::Create, AuthResource::Note(project(), None))
            .is_ok());
        assert!(s
            .can(AuthVerb::Update, AuthResource::Note(project(), None))
            .is_ok());
    }

    #[test]
    fn cross_project_admin_cannot_access_other_project() {
        let s = admin(project());
        assert!(s
            .can(AuthVerb::Read, AuthResource::Sandbox(other_ws(), None))
            .is_err());
        assert!(s
            .can(AuthVerb::Update, AuthResource::Skill(other_ws(), None))
            .is_err());
    }
}
