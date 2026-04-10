use rmcp::model::{CallToolResult, JsonObject, Tool};

use crate::auth::AuthContext;

use super::filter::OutputFilter;
use super::ToolSetsError;

pub struct ToolSetEntry {
    pub name: String,
    pub description: Tool,
    /// Optional default output filter for this tool (e.g. tail:150 for build logs).
    /// When set, this is used as the fallback before the global default.
    pub default_output_filter: Option<OutputFilter>,
}

#[async_trait::async_trait]
pub trait ToolSet: Send + Sync {
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
        auth: Option<&AuthContext>,
    ) -> Result<CallToolResult, ToolSetsError>;
}
