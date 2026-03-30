use thiserror::Error;

#[derive(Error, Debug)]
pub enum ToolSetsError {
    #[error("ToolSetsError - ClientInit: {0}")]
    ClientInit(#[from] rmcp::service::ClientInitializeError),
    #[error("ToolSetsError - Service: {0}")]
    Service(#[from] rmcp::service::ServiceError),
    #[error("ToolSetsError - InvalidHeader: {0}")]
    InvalidHeader(String),
    #[error("ToolSetsError - ToolNotFound: {0}")]
    ToolNotFound(String),
}
