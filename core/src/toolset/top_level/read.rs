//! `Read` — read a file with optional line range inside the agent's attached
//! sandbox. Thin alias for the text editor's `view` command: translates
//! `{path, offset, limit}` into `{command: "view", path, view_range}` and
//! forwards to the same `/execute` handler.
//!
//! Read-only: executable with either `SandboxUseAll` or `SandboxUseReadOnly`.

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use sandbox::instance_client::ExecuteRequest;
use serde::Deserialize;

use crate::auth::AuthSubject;
use crate::sandbox::Sandboxes;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{parse_params, schema_for};

// ---------------------------------------------------------------------------
// Params
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
struct ReadParams {
    /// Absolute path to the file inside the sandbox workspace.
    path: String,
    /// Line offset to start reading from (0-based). Optional.
    #[serde(default, deserialize_with = "super::liberal::deserialize_option_i64")]
    offset: Option<i64>,
    /// Maximum number of lines to read. Optional.
    #[serde(default, deserialize_with = "super::liberal::deserialize_option_i64")]
    limit: Option<i64>,
}

// ---------------------------------------------------------------------------
// Tool
// ---------------------------------------------------------------------------

pub struct Read {
    sandboxes: Arc<Sandboxes>,
}

impl Read {
    pub fn new(sandboxes: Arc<Sandboxes>) -> Self {
        Self { sandboxes }
    }
}

static READ_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<ReadParams>);

#[async_trait::async_trait]
impl TopLevelTool for Read {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        "Read a file with optional line range inside the agent's attached sandbox. \
         Read-only — works with both full and read-only sandbox attachments."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &READ_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        // Hidden from workspace admins — see bash.rs.
        subject.is_agent() && !subject.is_workspace_admin()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let sandbox_id = subject
            .readable_sandbox_id()
            .ok_or(ToolSetsError::Unauthorized)?;
        let params: ReadParams = parse_params(arguments)?;

        // Translate offset/limit into the text editor's view_range [start, end]
        // where start is 1-based and end = -1 means EOF.
        let mut editor_input = serde_json::json!({
            "command": "view",
            "path": params.path,
        });

        if params.offset.is_some() || params.limit.is_some() {
            let start = params.offset.unwrap_or(0) + 1; // 0-based offset → 1-based line
            let end = match params.limit {
                Some(l) => start + l - 1,
                None => -1, // EOF
            };
            editor_input["view_range"] = serde_json::json!([start, end]);
        }

        let client = self
            .sandboxes
            .instance_client_for(sandbox_id)
            .await
            .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;

        let req = ExecuteRequest {
            tool: "str_replace_based_edit_tool".to_string(),
            input: editor_input,
        };

        match client.execute(&req).await {
            Ok(resp) => {
                let content = vec![Content::text(resp.output)];
                if resp.is_error {
                    Ok(CallToolResult::error(content))
                } else {
                    Ok(CallToolResult::success(content))
                }
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "sandbox /execute call failed: {e}"
            ))])),
        }
    }
}
