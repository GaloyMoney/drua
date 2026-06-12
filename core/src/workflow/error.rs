use thiserror::Error;

use crate::agent::error::AgentError;
use crate::auth::error::AuthorizationError;
use crate::sandbox::SandboxError;

use super::repo::{
    WorkflowDefinitionCreateError, WorkflowDefinitionFindError, WorkflowDefinitionModifyError,
    WorkflowDefinitionQueryError,
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
    #[error("WorkflowError - DefinitionModify: {0}")]
    DefinitionModify(#[from] WorkflowDefinitionModifyError),
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
    #[error("WorkflowError - InvalidStep: {0}")]
    InvalidStep(String),
    #[error("WorkflowError - InvalidTemplateRef: {0}")]
    InvalidTemplateRef(String),
    #[error("WorkflowError - InvalidCondition: {0}")]
    InvalidCondition(String),
    #[error("WorkflowError - ConditionNotBoolean: step '{step}' condition `{body}` evaluated to {value} (expected bool)")]
    ConditionNotBoolean {
        step: String,
        body: String,
        value: String,
    },
    /// Trigger-level `condition:` failed at runtime — CEL compile,
    /// runtime, or non-boolean result. No run was created; an
    /// audit row was written.
    #[error(
        "WorkflowError - TriggerConditionErrored: trigger condition `{body}` failed: {reason}"
    )]
    TriggerConditionErrored { body: String, reason: String },
    #[error("WorkflowError - WorkflowPaused: workflow '{0}' is paused; resume it to trigger runs")]
    WorkflowPaused(String),
    #[error("WorkflowError - ToolNotFound: {0}")]
    ToolNotFound(String),
    #[error("WorkflowError - ToolDispatch: {0}")]
    ToolDispatch(String),
    #[error("WorkflowError - InvalidCronExpression: {0}")]
    InvalidCronExpression(String),
    #[error("WorkflowError - InvalidTimezone: {0}")]
    InvalidTimezone(String),
    #[error("WorkflowError - SkillNotFound: {0}")]
    SkillNotFound(String),
    #[error("WorkflowError - SandboxNotFound: {0}")]
    SandboxNotFound(String),
    #[error("WorkflowError - DuplicateSandboxName: {0}")]
    DuplicateSandboxName(String),
    #[error("WorkflowError - UndeclaredSandbox: step references undeclared sandbox: {0}")]
    UndeclaredSandbox(String),
    #[error("WorkflowError - SandboxNotReady: {name} is in state {state}")]
    SandboxNotReady { name: String, state: String },
    #[error("WorkflowError - StepErrored: {step}: {reason}")]
    StepErrored { step: String, reason: String },
    #[error("WorkflowError - Agent: {0}")]
    Agent(#[from] AgentError),
    #[error("WorkflowError - Skill: {0}")]
    Skill(String),
    #[error("WorkflowError - Sandbox: {0}")]
    Sandbox(#[from] SandboxError),
    #[error("WorkflowError - Job: {0}")]
    Job(String),
    #[error("WorkflowError - Library: {0}")]
    Library(#[from] drua_library::LibraryError),
    #[error("WorkflowError - Authorization: {0}")]
    Authorization(#[from] AuthorizationError),
    /// Cooperative cancellation. The runner observes the job's shutdown
    /// signal and returns this between safe checkpoints; the dispatcher
    /// reschedules the job so the next poller resumes from the persisted
    /// state.
    #[error("WorkflowError - Cancelled: graceful shutdown requested")]
    Cancelled,
}
