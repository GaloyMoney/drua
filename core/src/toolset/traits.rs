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

/// Dynamic-registration provenance carried by a [`SearchableToolSet`].
///
/// Returned by [`SearchableToolSet::scope`] so that the [`super::ToolSets`]
/// container can (a) atomically replace all toolsets owned by a particular
/// scope and (b) reject stale cleanup calls from an evicted owner. Static
/// toolsets (configured upstreams, Concourse, code-assistant, etc.) return
/// `None` and are never eligible for scope-based removal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolSetScope {
    /// A tunnel-registered toolset. Two keys — `deployment_id` identifies
    /// *which* tunnel's toolsets to atomically replace on takeover;
    /// `session_id` identifies *which specific session* is the current
    /// owner, so an evicted WS loop's late cleanup can be distinguished
    /// from the live session and ignored.
    Tunnel {
        deployment_id: String,
        session_id: uuid::Uuid,
    },
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

    /// Optional JSON Schema declaring the structure of this tool's output.
    /// When present the MCP gateway includes it in the tool definition and
    /// the tool MUST return `structured_content` in its `CallToolResult`.
    fn output_schema(&self) -> Option<&serde_json::Value> {
        None
    }

    /// Whether this tool should appear in `list_tools` / prompt tool arrays
    /// for the given subject. Default: always visible. Override to hide a
    /// tool (e.g. admin-only controls) without blocking execution.
    fn is_visible(&self, _subject: &AuthSubject) -> bool {
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

    /// Provenance for dynamically-registered toolsets. Static toolsets
    /// (configured upstreams, Concourse, code-assistant) leave this at
    /// the default `None`; tunnel-backed toolsets return `Some(..)` so
    /// that takeover can atomically replace them and stale cleanup can
    /// distinguish live from evicted ownership. See [`ToolSetScope`].
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
