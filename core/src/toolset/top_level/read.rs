//! `Read` — read a file with optional line range inside the agent's attached
//! sandbox. Thin alias for the text editor's `view` command: translates
//! `{path, offset, limit}` into `{command: "view", path, view_range}` and
//! forwards to the same `/execute` handler.
//!
//! Read-only: executable with either `SandboxUseAll` or `SandboxUseReadOnly`.

use std::sync::LazyLock;

use rmcp::model::{CallToolResult, Content, JsonObject};
use sandbox::instance_client::ExecuteRequest;

use crate::auth::AuthSubject;
use crate::sandbox::Sandboxes;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;

pub struct Read {
    sandboxes: Sandboxes,
}

impl Read {
    pub fn new(sandboxes: Sandboxes) -> Self {
        Self { sandboxes }
    }
}

static READ_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Absolute path to the file inside the sandbox workspace."
            },
            "offset": {
                "type": "integer",
                "description": "Line offset to start reading from (0-based). Optional."
            },
            "limit": {
                "type": "integer",
                "description": "Maximum number of lines to read. Optional."
            }
        },
        "required": ["path"],
        "additionalProperties": false,
    })
});

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
        subject.is_agent()
    }

    fn can_execute(&self, subject: &AuthSubject) -> bool {
        subject.is_agent() && subject.readable_sandbox_id().is_some()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let sandbox_id = subject.readable_sandbox_id().ok_or(ToolSetsError::Unauthorized)?;
        let args = arguments.unwrap_or_default();

        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolSetsError::MissingArgument("path".to_string()))?;

        // Translate offset/limit into the text editor's view_range [start, end]
        // where start is 1-based and end = -1 means EOF.
        let mut editor_input = serde_json::json!({
            "command": "view",
            "path": path,
        });

        let offset = args.get("offset").and_then(|v| v.as_i64());
        let limit = args.get("limit").and_then(|v| v.as_i64());

        if offset.is_some() || limit.is_some() {
            let start = offset.unwrap_or(0) + 1; // 0-based offset → 1-based line
            let end = match limit {
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
