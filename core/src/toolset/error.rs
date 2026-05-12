use thiserror::Error;

#[derive(Error, Debug)]
pub enum ToolSetsError {
    #[error("ToolSetsError - ClientInit: {0}")]
    ClientInit(#[from] Box<rmcp::service::ClientInitializeError>),
    #[error("ToolSetsError - Service: {0}")]
    Service(#[from] rmcp::service::ServiceError),
    #[error("ToolSetsError - MissingAuthHeader: upstream '{name}' requires authentication — set {env_key}")]
    MissingAuthHeader { name: String, env_key: String },
    #[error("ToolSetsError - GithubAppNotConfigured: upstream '{0}' requested auth_mode=github_app but no GitHub App is configured")]
    GithubAppNotConfigured(String),
    #[error("ToolSetsError - GithubApp: {0}")]
    GithubApp(#[from] github_app::GitHubAppError),
    #[error("ToolSetsError - InvalidHeader: {0}")]
    InvalidHeader(String),
    #[error("ToolSetsError - ToolNotFound: {0}")]
    ToolNotFound(String),
    #[error("ToolSetsError - MissingArgument: {0}")]
    MissingArgument(String),
    #[error("ToolSetsError - Concourse: {0}")]
    Concourse(#[from] concourse_client::ConcourseError),
    #[error("ToolSetsError - Zenduty: {0}")]
    Zenduty(#[from] zenduty_client::ZendutyError),
    #[error("ToolSetsError - InvalidArgument: {0}")]
    InvalidArgument(String),
    #[error("ToolSetsError - CodeAssistant: {0}")]
    CodeAssistant(String),
    #[error("ToolSetsError - Audit: {0}")]
    Audit(#[from] crate::audit::AuditError),
    #[error("ToolSetsError - Agent: {0}")]
    Agent(String),
    #[error("ToolSetsError - Sandbox: {0}")]
    Sandbox(#[from] crate::sandbox::SandboxError),
    #[error("ToolSetsError - Project: {0}")]
    Project(#[from] crate::project::ProjectError),
    #[error("ToolSetsError - Tunnel: {0}")]
    Tunnel(String),
    #[error("ToolSetsError - Note: {0}")]
    Note(String),
    #[error("ToolSetsError - Skill: {0}")]
    Skill(String),
    #[error("ToolSetsError - Library: {0}")]
    Library(#[from] crate::library::LibraryError),
    #[error("ToolSetsError - ToolCaching: {0}")]
    ToolCaching(#[from] drua_tool_caching::ToolCachingError),
    #[error("ToolSetsError - Compose: {0}")]
    Compose(String),
    #[error("ToolSetsError - Workflow: {0}")]
    Workflow(String),
    #[error("ToolSetsError - Unauthorized")]
    Unauthorized,
}

/// Why a top-level tool can't be invoked from a workflow `tool_step`.
/// Returned by [`super::ToolSets::find_for_workflow`]; both
/// parse-time validation (`workflow create`/`update`) and runtime
/// dispatch (`Executor::execute_tool_step`) route through that
/// helper so the contract is declared in exactly one place.
#[derive(Error, Debug)]
pub enum WorkflowToolLookupError {
    #[error("tool '{0}' is not registered")]
    NotRegistered(String),
    #[error(
        "tool '{0}' is not composable; only `composable: true` tools can be called from a workflow step"
    )]
    NotComposable(String),
    #[error("tool '{0}' does not declare an output_schema; tool_step requires structured output")]
    NoOutputSchema(String),
}
