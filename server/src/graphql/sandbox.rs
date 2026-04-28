use std::sync::Arc;

use async_graphql::{ComplexObject, Enum, InputObject, SimpleObject};

use super::agent::SandboxAttachmentMode;
use super::primitives::*;

use drua_core::sandbox::Sandbox as DomainSandbox;
use drua_core::sandbox::SandboxState as DomainSandboxState;

#[derive(SimpleObject, Clone)]
#[graphql(complex)]
pub struct Sandbox {
    id: SandboxId,
    workspace_id: WorkspaceId,
    name: String,
    state: SandboxStateEnum,
    last_error: Option<String>,
    mount_path: String,

    #[graphql(skip)]
    pub(super) entity: Arc<DomainSandbox>,
}

#[ComplexObject]
impl Sandbox {
    async fn created_at(&self) -> Timestamp {
        self.entity.created_at().into()
    }

    async fn cpu(&self) -> &str {
        &self.entity.specs.cpu
    }

    async fn memory(&self) -> &str {
        &self.entity.specs.memory
    }

    async fn disk_size(&self) -> &str {
        &self.entity.specs.disk_size
    }

    async fn attached_agents(&self) -> Vec<SandboxAgentAttachment> {
        self.entity
            .attached_agents
            .iter()
            .map(|(agent_id, mode)| SandboxAgentAttachment {
                agent_id: *agent_id,
                mode: SandboxAttachmentMode::from(*mode),
            })
            .collect()
    }
}

impl From<DomainSandbox> for Sandbox {
    fn from(entity: DomainSandbox) -> Self {
        Self {
            id: entity.id,
            workspace_id: entity.workspace_id,
            name: entity.name.clone(),
            state: SandboxStateEnum::from(entity.state),
            last_error: entity.last_error.clone(),
            mount_path: entity.mount_path.clone(),
            entity: Arc::new(entity),
        }
    }
}

#[derive(SimpleObject, Clone)]
pub struct SandboxAgentAttachment {
    agent_id: AgentId,
    mode: SandboxAttachmentMode,
}

#[derive(Enum, Clone, Copy, PartialEq, Eq)]
pub enum SandboxStateEnum {
    Provisioning,
    Initializing,
    Ready,
    Suspended,
    Errored,
}

impl From<DomainSandboxState> for SandboxStateEnum {
    fn from(state: DomainSandboxState) -> Self {
        match state {
            DomainSandboxState::Provisioning => Self::Provisioning,
            DomainSandboxState::Initializing => Self::Initializing,
            DomainSandboxState::Ready => Self::Ready,
            DomainSandboxState::Suspended => Self::Suspended,
            DomainSandboxState::Errored => Self::Errored,
        }
    }
}

#[derive(Enum, Clone, Copy, PartialEq, Eq)]
pub enum SandboxCreateMode {
    Scratch,
    Repo,
}

#[derive(InputObject)]
pub struct SandboxCreateInput {
    pub workspace_id: WorkspaceId,
    pub name: String,
    pub mode: SandboxCreateMode,
    pub repo_url: Option<String>,
    pub branch: Option<String>,
    #[graphql(default_with = "\"500m\".to_string()")]
    pub cpu: String,
    #[graphql(default_with = "\"512Mi\".to_string()")]
    pub memory: String,
    #[graphql(default_with = "\"10Gi\".to_string()")]
    pub disk_size: String,
}

mutation_payload! { SandboxCreatePayload, sandbox: Sandbox }

#[derive(InputObject)]
pub struct SandboxSuspendInput {
    pub id: SandboxId,
}

mutation_payload! { SandboxSuspendPayload, sandbox: Sandbox }

#[derive(InputObject)]
pub struct SandboxRestartInput {
    pub id: SandboxId,
}

mutation_payload! { SandboxRestartPayload, sandbox: Sandbox }
