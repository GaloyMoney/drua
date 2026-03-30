use rmcp::model::{CallToolResult, JsonObject, Tool};

use super::ToolSetsError;

pub struct ToolSetEntry {
    pub name: String,
    pub description: Tool,
}

#[async_trait::async_trait]
pub trait ToolSet: Send + Sync {
    fn name(&self) -> &str;
    fn category(&self) -> Option<&str>;
    fn category_description(&self) -> Option<&str>;
    fn tools(&self) -> &[ToolSetEntry];
    async fn call(&self, tool_name: &str, arguments: Option<JsonObject>)
        -> Result<CallToolResult, ToolSetsError>;
}
