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
    ///
    /// Typed-output tools are exempt from the universal classify/persist/envelope
    /// stage in `ToolSets::call_top_level_tool`: replacing their
    /// `structured_content` with a recovery envelope would violate the advertised
    /// MCP `outputSchema` and cause clients/servers to reject the result.
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

    /// Skip the universal classify+persist+envelope stage; raw result is returned unchanged.
    fn bypass_universal_pipeline(&self) -> bool {
        false
    }

    // cursor bugbot #3210558743: when true, dispatcher's bypass branch skips
    // its default `record_pipeline_metrics(raw, raw, "bypass", false)` so an
    // internally-compressing tool doesn't record a misleading 1.0 ratio.
    fn records_own_pipeline_metrics(&self) -> bool {
        false
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
