use std::sync::Arc;

use async_graphql::{ComplexObject, Context, Enum, SimpleObject};

use super::primitives::*;

use drua_core::agent::Agent as DomainAgent;
use drua_core::agent::AgentRole as DomainAgentRole;
use drua_core::sandbox::SandboxAgentMode;

#[derive(SimpleObject, Clone)]
#[graphql(complex)]
pub struct Agent {
    id: AgentId,
    workspace_id: WorkspaceId,
    name: String,
    role: AgentRole,

    #[graphql(skip)]
    pub(super) entity: Arc<DomainAgent>,
}

#[ComplexObject]
impl Agent {
    /// The sandbox this agent is attached to, if any.
    async fn attached_sandbox(
        &self,
        ctx: &Context<'_>,
    ) -> async_graphql::Result<Option<SandboxAttachment>> {
        let (sandbox_id, mode) = match self.entity.attached_sandbox {
            Some(v) => v,
            None => return Ok(None),
        };
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let sandbox = app.sandboxes().find_by_id(sub, sandbox_id).await?;
        Ok(Some(SandboxAttachment {
            sandbox_id,
            name: sandbox.name.clone(),
            mode: SandboxAttachmentMode::from(mode),
        }))
    }
}

impl From<DomainAgent> for Agent {
    fn from(entity: DomainAgent) -> Self {
        Self {
            id: entity.id,
            workspace_id: entity.workspace_id,
            name: entity.name.clone(),
            role: AgentRole::from(entity.agent_role),
            entity: Arc::new(entity),
        }
    }
}

#[derive(SimpleObject, Clone)]
pub struct SandboxAttachment {
    sandbox_id: SandboxId,
    name: String,
    mode: SandboxAttachmentMode,
}

#[derive(Enum, Clone, Copy, PartialEq, Eq)]
pub enum SandboxAttachmentMode {
    Read,
    Write,
}

impl From<SandboxAgentMode> for SandboxAttachmentMode {
    fn from(mode: SandboxAgentMode) -> Self {
        match mode {
            SandboxAgentMode::Read => Self::Read,
            SandboxAgentMode::Write => Self::Write,
        }
    }
}

#[derive(Enum, Clone, Copy, PartialEq, Eq)]
pub enum AgentRole {
    WorkspaceLead,
    Agent,
}

impl From<DomainAgentRole> for AgentRole {
    fn from(role: DomainAgentRole) -> Self {
        match role {
            DomainAgentRole::WorkspaceLead => Self::WorkspaceLead,
            DomainAgentRole::Agent => Self::Agent,
        }
    }
}
