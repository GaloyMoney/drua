//! Terminal tool for workflow step agents.
//!
//! When a workflow step's agent calls `submit_output`, the agent's
//! persisted `output_schema` is read from the [`Agents`] service and
//! the tool's args are validated against it. A successful call writes
//! the validated payload as the session's terminal `OutputSubmitted`
//! event (handled in `AgentSession::add_tool_results`), which
//! transitions the session to `Done` and ends the agent's turn.
//!
//! The tool is registered globally on `ToolSets`; `is_visible` gates
//! it to subjects carrying [`AuthScope::WorkflowStepAgent`] so only
//! workflow step agents see it in their tool catalogue.

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};

use crate::agent::session::message::SUBMIT_OUTPUT_TOOL_NAME;
use crate::agent::Agents;
use crate::auth::{AuthScope, AuthSubject};

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;

pub struct SubmitOutputTool {
    agents: Arc<Agents>,
}

impl SubmitOutputTool {
    pub fn new(agents: Arc<Agents>) -> Self {
        Self { agents }
    }
}

/// Permissive placeholder schema: the per-step real schema is
/// substituted into the session's `tool_defs` at agent creation,
/// overriding this entry by name. The model never sees this; it
/// exists only to satisfy `TopLevelTool::input_schema`'s static-ref
/// signature for the global registration.
static PLACEHOLDER_INPUT_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "additionalProperties": true,
    })
});

#[async_trait::async_trait]
impl TopLevelTool for SubmitOutputTool {
    fn name(&self) -> &str {
        SUBMIT_OUTPUT_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Call this exactly once to record this step's structured \
         result and finish the step. The arguments must match the \
         step's declared output schema."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &PLACEHOLDER_INPUT_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.has_scope(&AuthScope::WorkflowStepAgent)
    }

    /// Terminal control-flow tool — never composed via the JS bridge.
    fn composable(&self) -> bool {
        false
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let agent_id = subject.acting_agent_id().ok_or_else(|| {
            ToolSetsError::InvalidArgument("submit_output requires an agent subject".to_string())
        })?;

        let schema = self
            .agents
            .output_schema_for_agent(agent_id)
            .await
            .map_err(|e| ToolSetsError::InvalidArgument(e.to_string()))?
            .ok_or_else(|| {
                ToolSetsError::InvalidArgument(
                    "submit_output was called by an agent with no persisted output_schema; \
                     this is a runtime invariant violation"
                        .to_string(),
                )
            })?;

        let args_value = serde_json::Value::Object(arguments.unwrap_or_default());

        if let Err(e) = schema.validate(&args_value) {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "submit_output args failed schema validation: {e}"
            ))]));
        }

        let mut result = CallToolResult::success(vec![Content::text("output recorded")]);
        result.structured_content = Some(args_value);
        Ok(result)
    }
}
