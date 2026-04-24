use thiserror::Error;

#[derive(Error, Debug)]
pub enum ToolSetsError {
    #[error("ToolSetsError - ClientInit: {0}")]
    ClientInit(#[from] Box<rmcp::service::ClientInitializeError>),
    #[error("ToolSetsError - Service: {0}")]
    Service(#[from] rmcp::service::ServiceError),
    #[error("ToolSetsError - MissingAuthHeader: upstream '{name}' requires authentication — set {env_key}")]
    MissingAuthHeader { name: String, env_key: String },
    #[error("ToolSetsError - InvalidHeader: {0}")]
    InvalidHeader(String),
    #[error("ToolSetsError - ToolNotFound: {0}")]
    ToolNotFound(String),
    #[error("ToolSetsError - MissingArgument: {0}")]
    MissingArgument(String),
    #[error("ToolSetsError - Concourse: {0}")]
    Concourse(#[from] concourse_client::ConcourseError),
    #[error("ToolSetsError - InvalidArgument: {0}")]
    InvalidArgument(String),
    #[error("ToolSetsError - CodeAssistant: {0}")]
    CodeAssistant(String),
    #[error("ToolSetsError - Audit: {0}")]
    Audit(#[from] crate::audit::AuditError),
    #[error("ToolSetsError - Agent: {0}")]
    Agent(String),
    #[error("ToolSetsError - Sandbox: {0}")]
    Sandbox(String),
    #[error("ToolSetsError - Workspace: {0}")]
    Workspace(String),
    #[error("ToolSetsError - Tunnel: {0}")]
    Tunnel(String),
    #[error("ToolSetsError - Note: {0}")]
    Note(String),
    #[error("ToolSetsError - Skill: {0}")]
    Skill(String),
    #[error("ToolSetsError - Unauthorized")]
    Unauthorized,
}
