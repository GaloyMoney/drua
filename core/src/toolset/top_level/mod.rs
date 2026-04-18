//! Built-in [`TopLevelTool`](super::traits::TopLevelTool) implementations.

use rmcp::model::JsonObject;

use super::error::ToolSetsError;

/// Deserialize tool arguments into a typed params struct.
///
/// Converts the raw `Option<JsonObject>` from [`TopLevelTool::call`] into `T`.
/// Missing arguments are treated as an empty object (suitable for tools with
/// all-optional fields).
pub(super) fn parse_params<T: serde::de::DeserializeOwned>(
    arguments: Option<JsonObject>,
) -> Result<T, ToolSetsError> {
    let value = serde_json::Value::Object(arguments.unwrap_or_default());
    serde_json::from_value(value).map_err(|e| ToolSetsError::InvalidArgument(e.to_string()))
}

mod agent;
mod bash;
mod catalog;
mod glob;
mod grep;
mod inspect;
mod log;
mod ls;
mod ping;
mod read;
mod sandbox;
mod text_editor;
mod workspace;

pub use agent::{
    AdminAgentAttachSandbox, AdminAgentCreate, AdminAgentDetachSandbox, AdminListAgents,
    WorkspaceAgentAttachSandbox, WorkspaceAgentCreate, WorkspaceAgentDetachSandbox,
    WorkspaceListAgents,
};
pub use bash::Bash;
pub use catalog::{CallCatalogTool, DescribeCatalogTool, SearchCatalog};
pub use glob::GlobTool;
pub use grep::Grep;
pub use inspect::{AdminInspectSandbox, WorkspaceInspectSandbox};
pub use log::{AdminAllLogs, WorkspaceLog};
pub use ls::Ls;
pub use ping::Ping;
pub use read::Read;
pub use sandbox::{
    AdminCreateSandbox, AdminGetSandbox, AdminListSandboxes, WorkspaceCreateSandbox,
    WorkspaceGetSandbox, WorkspaceListSandboxes,
};
pub use text_editor::TextEditor;
pub use workspace::{AdminWorkspaceCreate, AdminWorkspaceList};
