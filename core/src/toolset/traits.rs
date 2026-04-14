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

    /// Whether this tool should appear in `list_tools` / prompt tool arrays
    /// for the given subject. Default: always visible. Override to hide a
    /// tool (e.g. admin-only controls) without blocking execution.
    fn is_visible(&self, _subject: &AuthSubject) -> bool {
        true
    }

    /// Whether the subject may actually invoke this tool. Default: yes.
    /// `ToolSets::call_top_level_tool` enforces this before dispatch and
    /// surfaces `ToolSetsError::Unauthorized` on `false`.
    fn can_execute(&self, _subject: &AuthSubject) -> bool {
        true
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError>;
}

impl From<&dyn TopLevelTool> for llm::prompt::Tool {
    fn from(t: &dyn TopLevelTool) -> Self {
        llm::prompt::Tool {
            name: t.name().to_string(),
            description: Some(t.description().to_string()),
            input_schema: t.input_schema().clone(),
            cache_control: None,
        }
    }
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

    /// Whether this toolset should appear in the catalog (`search_tools`,
    /// `describe_tool`, prompt-tools) for the given subject. Default: always
    /// visible. Override to hide a toolset behind a scope or role.
    fn is_visible(&self, _subject: &AuthSubject) -> bool {
        true
    }

    /// Whether the subject may invoke any tool in this set. Default: yes.
    /// `CallCatalogTool` enforces this before dispatching and surfaces
    /// `ToolSetsError::Unauthorized` on `false`.
    fn can_execute(&self, _subject: &AuthSubject) -> bool {
        true
    }

    async fn call(
        &self,
        tool_name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError>;
}
