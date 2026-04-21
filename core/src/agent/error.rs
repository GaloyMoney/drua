use thiserror::Error;

use crate::auth::error::AuthorizationError;
use crate::primitives::{SandboxId, WorkspaceId};
use crate::sandbox::error::SandboxError;
use crate::skill::SkillError;

use super::repo::{AgentCreateError, AgentFindError, AgentModifyError, AgentQueryError};
use super::session::error::AgentSessionError;

#[derive(Error, Debug)]
pub enum AgentError {
    #[error("AgentError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("AgentError - Create: {0}")]
    Create(#[from] AgentCreateError),
    #[error("AgentError - Modify: {0}")]
    Modify(#[from] AgentModifyError),
    #[error("AgentError - Find: {0}")]
    Find(#[from] AgentFindError),
    #[error("AgentError - Query: {0}")]
    Query(#[from] AgentQueryError),
    #[error("AgentError - Session: {0}")]
    Session(#[from] AgentSessionError),
    #[error("AgentError - Sandbox: {0}")]
    Sandbox(#[from] SandboxError),
    #[error("AgentError - Skill: {0}")]
    Skill(#[from] SkillError),
    #[error("AgentError - prompt request channel closed")]
    PromptRequestChannelClosed,
    #[error("AgentError - role not configured: {0:?}")]
    RoleNotConfigured(super::entity::AgentRole),
    #[error("AgentError - model not configured: {0}")]
    ModelNotConfigured(String),
    #[error("AgentError - Authorization: {0}")]
    Authorization(#[from] AuthorizationError),
    #[error("AgentError - unauthorized")]
    Unauthorized,
    #[error(
        "AgentError - agent is already attached to sandbox {current}; detach it before attaching another"
    )]
    AlreadyAttachedToSandbox { current: SandboxId },
    #[error("AgentError - workspace lead cannot attach a sandbox")]
    LeadCannotAttachSandbox,
    #[error("AgentError - no lead agent found in workspace {0}")]
    NoLeadAgent(WorkspaceId),
}
