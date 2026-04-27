use thiserror::Error;

use crate::auth::error::AuthorizationError;

use super::repo::{
    WorkflowDefinitionCreateError, WorkflowDefinitionFindError, WorkflowDefinitionQueryError,
};
use super::run::repo::{
    WorkflowRunCreateError, WorkflowRunFindError, WorkflowRunModifyError, WorkflowRunQueryError,
};

#[derive(Error, Debug)]
pub enum WorkflowError {
    #[error("WorkflowError - DefinitionCreate: {0}")]
    DefinitionCreate(#[from] WorkflowDefinitionCreateError),
    #[error("WorkflowError - DefinitionFind: {0}")]
    DefinitionFind(#[from] WorkflowDefinitionFindError),
    #[error("WorkflowError - DefinitionQuery: {0}")]
    DefinitionQuery(#[from] WorkflowDefinitionQueryError),
    #[error("WorkflowError - RunCreate: {0}")]
    RunCreate(#[from] WorkflowRunCreateError),
    #[error("WorkflowError - RunFind: {0}")]
    RunFind(#[from] WorkflowRunFindError),
    #[error("WorkflowError - RunModify: {0}")]
    RunModify(#[from] WorkflowRunModifyError),
    #[error("WorkflowError - RunQuery: {0}")]
    RunQuery(#[from] WorkflowRunQueryError),
    #[error("WorkflowError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("WorkflowError - BuildEntity: {0}")]
    BuildEntity(String),
    #[error("WorkflowError - InvalidDefinition: {0}")]
    InvalidDefinition(String),
    #[error("WorkflowError - SkillNotFound: {0}")]
    SkillNotFound(String),
    #[error("WorkflowError - SandboxNotFound: {0}")]
    SandboxNotFound(String),
    #[error("WorkflowError - StepFailed: {step}: {reason}")]
    StepFailed { step: String, reason: String },
    #[error("WorkflowError - Agent: {0}")]
    Agent(String),
    #[error("WorkflowError - Skill: {0}")]
    Skill(String),
    #[error("WorkflowError - Sandbox: {0}")]
    Sandbox(String),
    #[error("WorkflowError - Job: {0}")]
    Job(String),
    #[error("WorkflowError - Authorization: {0}")]
    Authorization(#[from] AuthorizationError),
}
