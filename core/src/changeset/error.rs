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
    #[error("ChangesetError - UnsupportedActor: subject cannot open or act on a changeset")]
    UnsupportedActor,
    #[error("ChangesetError - Foreign: changeset {id} belongs to another project")]
    Foreign { id: ChangesetId },
    #[error("ChangesetError - Authorization: {0}")]
    Authorization(#[from] AuthorizationError),
    #[error("ChangesetError - Forbidden: subject may not {action} changeset {id}")]
    Forbidden {
        id: ChangesetId,
        action: &'static str,
    },
    #[error("ChangesetError - GitHubApp: {0}")]
    GitHubApp(#[from] GitHubAppError),
    #[error("ChangesetError - Empty: changeset {id} has no commits to submit")]
    Empty { id: ChangesetId },
    #[error("ChangesetError - Conflicts: changeset {id} does not merge cleanly: {paths:?}")]
    Conflicts { id: ChangesetId, paths: Vec<String> },
    #[error(
        "ChangesetError - PrUnavailable: no GitHub App / GitHub remote configured for this library; use `apply` instead of `submit`"
    )]
    PrUnavailable,
    #[error("ChangesetError - ChangesetNotOpen: changeset {id} is {status}; only open changesets accept edits")]
    ChangesetNotOpen { id: String, status: String },
    #[error(
        "ChangesetError - ReadOnly: space:{slug}/ is read-only for this subject — spaces are edited by workflows (`space_writes:`) or by admins (`drua_admin_spaces` with target: draft)"
    )]
    ReadOnly { slug: String },
    #[error(
        "ChangesetError - DraftOpen: you have an open draft {id} \"{title}\"; write draft:{slug}/<path> (or pass target: draft to drua_admin_spaces) to keep staging, or run `drua_admin_spaces merge-draft` / `discard-draft` before writing space:{slug}/ directly"
    )]
    DraftOpen {
        id: String,
        title: String,
        slug: String,
    },
    #[error("ChangesetError - RunReadOnly: space:{slug}/ is read-only in this workflow run — set `space_writes.mode` to `merge` or `open_pr` in the workflow definition to let its steps write")]
    RunReadOnly { slug: String },
}
