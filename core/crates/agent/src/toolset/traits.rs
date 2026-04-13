use rmcp::model::{CallToolResult, JsonObject, Tool};

use crate::auth::AuthSubject;

use super::filter::OutputFilter;
use super::ToolSetsError;


pub struct ToolSetEntry {
    pub name: String,
    pub description: Tool,
    /// Optional default output filter for this tool (e.g. tail:150 for build logs).
    /// When set, this is used as the fallback before the global default.
    pub default_output_filter: Option<OutputFilter>,
}

/// A single tool exposed to the agent at the top level — e.g. `search_tools`,
/// `describe_tool`, `call_tool`, or later built-ins like `read` / `bash`.
///
/// Unlike [`SearchableToolSet`] which bundles many upstream tools behind a
/// prefix, a `TopLevelTool` is one tool, resolved by its exact name.
#[async_trait::async_trait]
pub trait TopLevelTool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> &serde_json::Value;

    /// Decide whether a caller with the given `scopes` is allowed to invoke
    /// this tool with `arguments`. Default: unrestricted. Override to make
    /// argument-aware authorization decisions (e.g. `call_tool` checking the
    /// inner toolset's required scopes).
    fn is_authorized(&self, _scopes: &[&str], _arguments: Option<&JsonObject>) -> bool {
        true
    }

    async fn call(
        &self,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError>;
}

#[async_trait::async_trait]
pub trait SearchableToolSet: Send + Sync {
    fn name(&self) -> &str;
    /// Prefix used for tool names in the catalog.  Defaults to `name()`.
    fn prefix(&self) -> &str {
        self.name()
    }
    fn category(&self) -> &str;
    fn category_description(&self) -> &str;
    fn tools(&self) -> &[ToolSetEntry];

    /// Scopes required to access this toolset. Empty means unrestricted.
    fn required_scopes(&self) -> &[&str] {
        &[]
    }

    async fn call(
        &self,
        tool_name: &str,
        arguments: Option<JsonObject>,
        auth: Option<&AuthSubject>,
    ) -> Result<CallToolResult, ToolSetsError>;
}
