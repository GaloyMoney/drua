use thiserror::Error;

use crate::agent::error::AgentError;
use crate::agent::repo::{AgentFindError, AgentModifyError};
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
}
