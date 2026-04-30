use crate::primitives::{
    AgentId, McpCredsId, NoteId, ProjectId, ProjectSecretId, SandboxId, SkillId, SpaceId,
    WorkflowDefinitionId,
};

/// Each variant encodes its parent container so authorization checks
/// don't need a separate "container" parameter.
///
/// Child IDs: `None` = "the collection" (for `Create`), `Some` = a
/// specific resource (for `Read`/`Update`/`Delete`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthResource {
    Project(Option<ProjectId>),
    Agent(ProjectId, Option<AgentId>),
    Sandbox(ProjectId, Option<SandboxId>),
    ProjectSecret(ProjectId, Option<ProjectSecretId>),
    /// User-scoped. Only `User` subjects and `Admin`-scoped agents can access.
    McpCreds(Option<McpCredsId>),
    Note(ProjectId, Option<NoteId>),
    Skill(ProjectId, Option<SkillId>),
    Workflow(ProjectId, Option<WorkflowDefinitionId>),
    AuditLog(ProjectId),
    /// Library-wide; not project-scoped.
    Space(Option<SpaceId>),
    /// Matched by [`super::AuthScope::External`] scopes by name.
    External(String),
}

impl AuthResource {
    pub fn project_id(&self) -> Option<ProjectId> {
        match self {
            AuthResource::Project(project) => *project,
            AuthResource::Agent(project, _)
            | AuthResource::Sandbox(project, _)
            | AuthResource::ProjectSecret(project, _)
            | AuthResource::Note(project, _)
            | AuthResource::Skill(project, _)
            | AuthResource::Workflow(project, _)
            | AuthResource::AuditLog(project) => Some(*project),
            AuthResource::McpCreds(_) | AuthResource::Space(_) | AuthResource::External(_) => None,
        }
    }
}
