use std::sync::Arc;

use async_graphql::{ComplexObject, Context, Enum, InputObject, SimpleObject};

use super::primitives::*;

use drua_core::agent::Agent as DomainAgent;
use drua_core::agent::AgentRole as DomainAgentRole;
use drua_core::sandbox::SandboxAgentMode;
use drua_core::{ModelChain as DomainModelChain, ModelSpec as DomainModelSpec};

#[derive(SimpleObject, InputObject, Clone)]
#[graphql(input_name = "ModelSpecInput")]
pub struct ModelSpec {
    pub name: String,
    pub max_tokens: Option<i32>,
}

impl From<DomainModelSpec> for ModelSpec {
    fn from(s: DomainModelSpec) -> Self {
        Self {
            name: s.name,
            max_tokens: s.max_tokens.map(|n| n as i32),
        }
    }
}

impl From<ModelSpec> for DomainModelSpec {
    fn from(s: ModelSpec) -> Self {
        DomainModelSpec {
            name: s.name,
            max_tokens: s.max_tokens.map(|n| n.max(0) as u32),
        }
    }
}

#[derive(SimpleObject, InputObject, Clone)]
#[graphql(input_name = "ModelChainInput")]
pub struct ModelChain {
    pub primary: ModelSpec,
    pub fallbacks: Vec<ModelSpec>,
}

impl From<DomainModelChain> for ModelChain {
    fn from(c: DomainModelChain) -> Self {
        Self {
            primary: c.primary.into(),
            fallbacks: c.fallbacks.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<ModelChain> for DomainModelChain {
    fn from(c: ModelChain) -> Self {
        DomainModelChain {
            primary: c.primary.into(),
            fallbacks: c.fallbacks.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(SimpleObject, Clone)]
#[graphql(complex)]
pub struct Agent {
    id: AgentId,
    project_id: ProjectId,
    name: String,
    role: AgentRole,

    #[graphql(skip)]
    pub(super) entity: Arc<DomainAgent>,
}

#[ComplexObject]
impl Agent {
    /// `None` ⇒ use the project / role / config-default chain.
    /// Always `None` for workflow-spawned agents.
    async fn model_chain_override(&self) -> Option<ModelChain> {
        self.entity.model_chain_override.clone().map(Into::into)
    }

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

    async fn session(
        &self,
        ctx: &Context<'_>,
    ) -> async_graphql::Result<super::session::AgentSession> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let session = app.agents().find_session(sub, self.entity.id).await?;
        Ok(super::session::AgentSession::from(session))
    }
}

impl From<DomainAgent> for Agent {
    fn from(entity: DomainAgent) -> Self {
        Self {
            id: entity.id,
            project_id: entity.project_id,
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
    ProjectLead,
    Agent,
    WorkflowStepAgent,
}

impl From<DomainAgentRole> for AgentRole {
    fn from(role: DomainAgentRole) -> Self {
        match role {
            DomainAgentRole::ProjectLead => Self::ProjectLead,
            DomainAgentRole::Agent => Self::Agent,
            DomainAgentRole::WorkflowStepAgent => Self::WorkflowStepAgent,
        }
    }
}

impl From<SandboxAttachmentMode> for SandboxAgentMode {
    fn from(mode: SandboxAttachmentMode) -> Self {
        match mode {
            SandboxAttachmentMode::Read => Self::Read,
            SandboxAttachmentMode::Write => Self::Write,
        }
    }
}

#[derive(InputObject)]
pub struct AgentCreateInput {
    pub project_id: ProjectId,
    pub name: String,
    pub sandbox_id: Option<SandboxId>,
    pub sandbox_mode: Option<SandboxAttachmentMode>,
    /// Initial chain override; `None` falls through to the project's
    /// override, then role, then `agents.default_chain`.
    pub model_chain: Option<ModelChain>,
}

mutation_payload! { AgentCreatePayload, agent: Agent }

#[derive(InputObject)]
pub struct AgentUpdateModelChainInput {
    pub agent_id: AgentId,
    /// `None` clears the override (revert to project / role / default).
    /// Rejects workflow-spawned agents.
    pub chain: Option<ModelChain>,
}

mutation_payload! { AgentUpdateModelChainPayload, agent: Agent }

#[derive(InputObject)]
pub struct AgentAttachSandboxInput {
    pub agent_id: AgentId,
    pub sandbox_id: SandboxId,
    pub mode: SandboxAttachmentMode,
}

mutation_payload! { AgentAttachSandboxPayload, agent: Agent }

#[derive(InputObject)]
pub struct AgentDetachSandboxInput {
    pub agent_id: AgentId,
    pub sandbox_id: SandboxId,
}

mutation_payload! { AgentDetachSandboxPayload, agent: Agent }

#[derive(InputObject)]
pub struct AgentDeleteInput {
    pub id: AgentId,
}

#[derive(async_graphql::SimpleObject)]
pub struct AgentDeletePayload {
    pub deleted_id: AgentId,
}
