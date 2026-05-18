use rmcp::model::{CallToolResult, JsonObject, Tool};

use crate::auth::AuthSubject;

use super::{ToolSetScope, ToolSetsError};

pub struct ToolSetEntry {
    pub name: String,
    pub description: Tool,
}

#[async_trait::async_trait]
pub trait TopLevelTool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> &serde_json::Value;

    /// The tool's upstream-shaped output schema. Implementors override
    /// this with their raw schema; `output_schema()` is what callers
    /// read — it adds the `DruaToolResult<T>` wrapper when the tool
    /// participates in tool-caching (`default_tool_caching() == true`).
    fn inner_output_schema(&self) -> Option<&serde_json::Value> {
        None
    }

    /// What MCP clients see in `tools/list`, what `compose_types`
    /// generates TS from, and what `describe_tool` advertises. Default
    /// impl wraps `inner_output_schema()` when the tool is cached, so
    /// every call site reads one consistent shape.
    fn output_schema(&self) -> Option<serde_json::Value> {
        let inner = self.inner_output_schema()?;
        if self.default_tool_caching() {
            Some(super::wrap::wrap_output_schema(inner))
        } else {
            Some(inner.clone())
        }
    }

    fn is_visible(&self, _subject: &AuthSubject) -> bool {
        true
    }

    /// Whether the tool can be invoked from a `compose` JS script.
    fn composable(&self) -> bool {
        true
    }

    /// True (default): the top-level dispatcher runs `ToolCaching` on the
    /// result, and `output_schema()` is wrapped in `DruaToolResult<T>`.
    /// False: the tool owns its own envelope shape (`compose`, `call_tool`,
    /// `tool_output_fetch`) — wrapping would shadow its native fields or
    /// recursively cache.
    fn default_tool_caching(&self) -> bool {
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
            strict: false,
        }
    }
}

#[async_trait::async_trait]
pub trait SearchableToolSet: Send + Sync {
    fn name(&self) -> &str;
    fn prefix(&self) -> &str {
        self.name()
    }
    fn category(&self) -> &str;
    fn category_description(&self) -> &str;
    fn tools(&self) -> &[ToolSetEntry];

    fn is_visible(&self, _subject: &AuthSubject) -> bool {
        true
    }

    /// `Some(..)` for dynamically-registered toolsets (e.g. tunnels).
    fn scope(&self) -> Option<&ToolSetScope> {
        None
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        tool_name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError>;
}
