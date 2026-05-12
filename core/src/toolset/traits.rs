use rmcp::model::{CallToolResult, JsonObject, Tool};

use crate::auth::AuthSubject;

use super::ToolSetsError;

pub struct ToolSetEntry {
    pub name: String,
    pub description: Tool,
}

/// Dynamic-registration provenance for atomic scope-based replacement of toolsets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolSetScope {
    Tunnel {
        deployment_id: String,
        session_id: uuid::Uuid,
    },
}

#[async_trait::async_trait]
pub trait TopLevelTool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> &serde_json::Value;

    /// When present, the tool MUST return `structured_content` in its `CallToolResult`.
    fn output_schema(&self) -> Option<&serde_json::Value> {
        None
    }

    fn is_visible(&self, _subject: &AuthSubject) -> bool {
        true
    }

    /// Whether the tool can be invoked from a `compose` JS script.
    fn composable(&self) -> bool {
        true
    }

    /// True (default): the top-level dispatcher runs `ToolCaching` on the
    /// result. False: the tool calls `ToolCaching` itself (e.g. because it
    /// wants to attribute under a different `tool_name` or run it on
    /// sub-results internally).
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
