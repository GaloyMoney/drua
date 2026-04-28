use crate::primitives::{
    AgentId, McpCredsId, NoteId, SandboxId, SkillId, WorkflowDefinitionId, WorkspaceId,
    WorkspaceSecretId,
};

/// Each variant encodes its parent container so authorization checks
/// don't need a separate "container" parameter.
///
/// Child IDs: `None` = "the collection" (for `Create`), `Some` = a
/// specific resource (for `Read`/`Update`/`Delete`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthResource {
    Workspace(Option<WorkspaceId>),
    Agent(WorkspaceId, Option<AgentId>),
    Sandbox(WorkspaceId, Option<SandboxId>),
    WorkspaceSecret(WorkspaceId, Option<WorkspaceSecretId>),
    /// User-scoped. Only `User` subjects and `Admin`-scoped agents can access.
    McpCreds(Option<McpCredsId>),
    Note(WorkspaceId, Option<NoteId>),
    Skill(WorkspaceId, Option<SkillId>),
    Workflow(WorkspaceId, Option<WorkflowDefinitionId>),
    AuditLog(WorkspaceId),
    /// Matched by [`super::AuthScope::External`] scopes by name.
    External(String),
}

impl AuthResource {
    pub fn workspace_id(&self) -> Option<WorkspaceId> {
        match self {
            AuthResource::Workspace(ws) => *ws,
            AuthResource::Agent(ws, _)
            | AuthResource::Sandbox(ws, _)
            | AuthResource::WorkspaceSecret(ws, _)
            | AuthResource::Note(ws, _)
            | AuthResource::Skill(ws, _)
            | AuthResource::Workflow(ws, _)
            | AuthResource::AuditLog(ws) => Some(*ws),
            AuthResource::McpCreds(_) | AuthResource::External(_) => None,
        }
    }
}
