use thiserror::Error;

use sandbox::{AdminError, InstanceError};

use crate::auth::error::AuthorizationError;
use crate::primitives::{AgentId, ProjectId};

use super::repo::{SandboxCreateError, SandboxFindError, SandboxModifyError, SandboxQueryError};

#[derive(Error, Debug)]
pub enum SandboxError {
    #[error("SandboxError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("SandboxError - Create: {0}")]
    Create(#[from] SandboxCreateError),
    #[error("SandboxError - Modify: {0}")]
    Modify(#[from] SandboxModifyError),
    #[error("SandboxError - Find: {0}")]
    Find(#[from] SandboxFindError),
    #[error("SandboxError - Query: {0}")]
    Query(#[from] SandboxQueryError),
    #[error("SandboxError - Admin: {0}")]
    Admin(#[from] AdminError),
    #[error("SandboxError - Instance: {0}")]
    Instance(#[from] InstanceError),
    #[error("SandboxError - Hydration: {0}")]
    Hydration(#[from] es_entity::EntityHydrationError),
    #[error("SandboxError - sandbox does not belong to project {expected} (actual: {actual})")]
    WrongProject {
        expected: ProjectId,
        actual: ProjectId,
    },
    #[error("SandboxError - write slot already taken by agent {current_writer}")]
    WriteSlotTaken { current_writer: AgentId },
    #[error("SandboxError - sandbox is in {state} state and has no live instance (must be Ready)")]
    NotReady { state: String },
    #[error("SandboxError - no sandbox named '{name}' in project {project_id}")]
    NameNotFound { project_id: ProjectId, name: String },
    #[error("SandboxError - Authorization: {0}")]
    Authorization(#[from] AuthorizationError),
    /// Repo URL didn't parse as a recognised github clone URL. Surfaces
    /// as a 400-equivalent at every API surface so the agent gets a
    /// clear message instead of a stuck-Errored sandbox.
    #[error("SandboxError - InvalidRepoUrl: '{url}' is not a recognised github clone URL")]
    InvalidRepoUrl { url: String },
    /// Repo URL parsed but isn't in the global git-proxy allow-list
    /// (or is configured pull-disabled). The sandbox would have failed
    /// to clone the moment its tool-server reached the proxy; pre-
    /// validating at create time keeps `Errored` rows + leaked
    /// tool-server children out of the picture entirely.
    #[error("SandboxError - RepoNotAllowed: {url} is not permitted by the git-proxy allow-list ({reason})")]
    RepoNotAllowed { url: String, reason: String },
    /// K8s storage providers don't support PVC shrinks; rejecting at
    /// the service layer keeps the entity row consistent with the
    /// backing PVC's effective size.
    #[error(
        "SandboxError - DiskShrinkNotSupported: cannot shrink disk from {current} to {requested}"
    )]
    DiskShrinkNotSupported { current: String, requested: String },
}
