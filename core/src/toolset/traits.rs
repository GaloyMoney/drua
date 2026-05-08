use rmcp::model::{CallToolResult, JsonObject, Tool};

use crate::auth::AuthSubject;

use super::filter::OutputFilter;
use super::ToolSetsError;

pub struct ToolSetEntry {
    pub name: String,
    pub description: Tool,
    /// Optional per-tool default (e.g. tail:150 for build logs); fallback before the global default.
    pub default_output_filter: Option<OutputFilter>,
}

/// Dynamic-registration provenance: lets the container atomically replace all
/// toolsets in a scope and reject stale cleanup calls from an evicted owner.
/// Static toolsets return `None` and are never eligible for scope-based removal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolSetScope {
    /// `deployment_id` selects *which* tunnel's toolsets to replace on takeover;
    /// `session_id` identifies the current owner, so an evicted WS loop's late
    /// cleanup can be distinguished from the live session.
    Tunnel {
        deployment_id: String,
        session_id: uuid::Uuid,
    },
}

/// One tool resolved by exact name. Unlike [`SearchableToolSet`] which
/// bundles many upstream tools behind a prefix.
#[async_trait::async_trait]
pub trait TopLevelTool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> &serde_json::Value;

    /// When present the MCP gateway includes it in the tool definition and
    /// the tool MUST return `structured_content` in its `CallToolResult`.
    fn output_schema(&self) -> Option<&serde_json::Value> {
        None
    }

    /// Default: always visible. Override to hide without blocking execution.
    fn is_visible(&self, _subject: &AuthSubject) -> bool {
        true
    }

    /// Default `true`. Override to `false` only for meta-tools whose
    /// semantics don't fit a JS bridge — `compose` (recursion),
    /// `compose_types` (introspection-only), and `use_skill` (spawns a
    /// nested agent). Project-admin tools (`agent`, `sandbox`,
    /// `skill`, `workflow`, ...) ARE composable: their service methods
    /// run `subject.can(...)` themselves and the dispatcher passes the
    /// caller's subject through unchanged, so authz is preserved.
    fn composable(&self) -> bool {
        true
    }

    /// `true` opts the tool's result out of the universal-pipeline
    /// classify+persist+envelope stage in `ToolSets::call_top_level_tool`
    /// — the raw `CallToolResult` is returned unchanged. Set on
    /// `tool_output_fetch` so the recovery escape hatch's own response
    /// (which can itself be tens of KB, deliberately) doesn't get
    /// re-classified as `Generic` and wrapped in a fresh envelope, which
    /// would defeat the point of the fetch tool.
    fn bypass_universal_pipeline(&self) -> bool {
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
    /// Defaults to `name()`.
    fn prefix(&self) -> &str {
        self.name()
    }
    fn category(&self) -> &str;
    fn category_description(&self) -> &str;
    fn tools(&self) -> &[ToolSetEntry];

    /// Default: always visible. Override to hide behind a scope or role.
    fn is_visible(&self, _subject: &AuthSubject) -> bool {
        true
    }

    /// `Some(..)` for dynamically-registered toolsets (e.g. tunnels);
    /// static toolsets return `None`. See [`ToolSetScope`].
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
