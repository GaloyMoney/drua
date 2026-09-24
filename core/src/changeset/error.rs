use thiserror::Error;

use crate::agent::error::AgentError;
use crate::agent::repo::{AgentFindError, AgentModifyError};
use crate::auth::error::AuthorizationError;
use crate::github_app::GitHubAppError;
use crate::primitives::ChangesetId;
use crate::workflow::run::repo::{WorkflowRunFindError, WorkflowRunModifyError};

use super::entity::ChangesetStatus;
use super::repo::{
    ChangesetCreateError, ChangesetFindError, ChangesetModifyError, ChangesetQueryError,
};

#[derive(Error, Debug)]
pub enum ChangesetError {
    #[error("ChangesetError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("ChangesetError - Create: {0}")]
    Create(#[from] ChangesetCreateError),
    #[error("ChangesetError - Find: {0}")]
    Find(#[from] ChangesetFindError),
    #[error("ChangesetError - Modify: {0}")]
    Modify(#[from] ChangesetModifyError),
    #[error("ChangesetError - Query: {0}")]
    Query(#[from] ChangesetQueryError),
    #[error("ChangesetError - Library: {0}")]
    Library(#[from] drua_library::LibraryError),
    #[error("ChangesetError - Agent: {0}")]
    Agent(#[from] AgentError),
    #[error("ChangesetError - AgentFind: {0}")]
    AgentFind(#[from] AgentFindError),
    #[error("ChangesetError - AgentModify: {0}")]
    AgentModify(#[from] AgentModifyError),
    #[error("ChangesetError - WorkflowRunFind: {0}")]
    WorkflowRunFind(#[from] WorkflowRunFindError),
    #[error("ChangesetError - WorkflowRunModify: {0}")]
    WorkflowRunModify(#[from] WorkflowRunModifyError),
    #[error("ChangesetError - InvalidTransition: {op} is not valid from {from:?}")]
    InvalidTransition {
        from: ChangesetStatus,
        op: &'static str,
    },
    #[error("ChangesetError - NoProject: subject has no project context")]
    NoProject,
    #[error("ChangesetError - UnsupportedActor: subject cannot open or act on a changeset")]
    UnsupportedActor,
    #[error("ChangesetError - MainUnborn: library repo has no commits on main yet")]
    MainUnborn,
    #[error("ChangesetError - Foreign: changeset {id} belongs to another project")]
    Foreign { id: ChangesetId },
    #[error("ChangesetError - Authorization: {0}")]
    Authorization(#[from] AuthorizationError),
    /// Role-based checks that don't reduce to a single `AuthVerb` (e.g.
    /// `discard`'s "bound actor, or a lead/admin" rule — §7, OQ-1).
    #[error("ChangesetError - Forbidden: subject may not {action} changeset {id}")]
    Forbidden {
        id: ChangesetId,
        action: &'static str,
    },
    #[error("ChangesetError - GitHubApp: {0}")]
    GitHubApp(#[from] GitHubAppError),
    /// `submit` on a changeset with no `CommitRecorded` events yet —
    /// §7: "if commit_count == 0 → Empty".
    #[error("ChangesetError - Empty: changeset {id} has no commits to submit")]
    Empty { id: ChangesetId },
    /// `submit`/`apply`/`rebase` found the changeset doesn't merge
    /// cleanly onto `main`'s current tip; the git op is never
    /// attempted (§7).
    #[error("ChangesetError - Conflicts: changeset {id} does not merge cleanly: {paths:?}")]
    Conflicts { id: ChangesetId, paths: Vec<String> },
    /// `submit` when the library has no GitHub App configured (local
    /// dev) or `repo_url` isn't a `github.com` remote — OQ-14's
    /// default: error, not a silent degrade to `apply`.
    #[error(
        "ChangesetError - PrUnavailable: no GitHub App / GitHub remote configured for this library; use `apply` instead of `submit`"
    )]
    PrUnavailable,
}
