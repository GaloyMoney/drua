use crate::primitives::{
    AgentId, McpCredsId, NoteId, SandboxId, SkillId, WorkspaceId, WorkspaceSecretId,
};

// `External` carries a `String` so `AuthResource` cannot be `Copy`.
// All other variants remain `Copy`-friendly.

/// What is being acted upon. Each variant encodes its parent container so
/// that authorization checks can verify the subject has access to the right
/// scope without a separate "container" parameter.
///
/// Child IDs are `Option` — `None` means "the collection" (used for
/// `Create` checks where no ID exists yet), `Some` means "this specific
/// resource" (used for `Read`/`Update`/`Delete`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthResource {
    /// A workspace. `None` means "create any workspace" (system-level),
    /// `Some` means a specific workspace.
    Workspace(Option<WorkspaceId>),
    /// An agent inside a workspace.
    Agent(WorkspaceId, Option<AgentId>),
    /// A sandbox inside a workspace.
    Sandbox(WorkspaceId, Option<SandboxId>),
    /// A secret inside a workspace.
    WorkspaceSecret(WorkspaceId, Option<WorkspaceSecretId>),
    /// MCP credentials. User-scoped (no workspace). Only `User` subjects
    /// and `Admin`-scoped agents can access these.
    McpCreds(Option<McpCredsId>),
    /// A note inside a workspace.
    Note(WorkspaceId, Option<NoteId>),
    /// A skill inside a workspace.
    Skill(WorkspaceId, Option<SkillId>),
    /// The audit log for a workspace.
    AuditLog(WorkspaceId),
    /// An externally-defined resource. Matches [`super::AuthScope::External`]
    /// scopes by name.
    External(String),
}

impl AuthResource {
    /// The workspace this resource lives in, if any.
    pub fn workspace_id(&self) -> Option<WorkspaceId> {
        match self {
            AuthResource::Workspace(ws) => *ws,
            AuthResource::Agent(ws, _)
            | AuthResource::Sandbox(ws, _)
            | AuthResource::WorkspaceSecret(ws, _)
            | AuthResource::Note(ws, _)
            | AuthResource::Skill(ws, _)
            | AuthResource::AuditLog(ws) => Some(*ws),
            AuthResource::McpCreds(_) | AuthResource::External(_) => None,
        }
    }
}
