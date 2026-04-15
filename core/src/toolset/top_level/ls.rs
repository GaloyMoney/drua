//! `LS` — directory listing inside the agent's attached sandbox.
//! Thin alias for the text editor's `view` command on a directory path:
//! translates `{path, ignore}` into `{command: "view", path}` and
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

pub struct Ls {
    sandboxes: Sandboxes,
}

impl Ls {
    pub fn new(sandboxes: Sandboxes) -> Self {
        Self { sandboxes }
    }
}

static LS_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Absolute path to the directory inside the sandbox workspace."
            },
            "ignore": {
                "type": "array",
                "items": { "type": "string" },
                "description": "List of file/directory names to exclude from the listing."
            }
        },
        "required": ["path"],
        "additionalProperties": false,
    })
});

#[async_trait::async_trait]
impl TopLevelTool for Ls {
    fn name(&self) -> &str {
        "LS"
    }

    fn description(&self) -> &str {
        "List directory contents inside the agent's attached sandbox. \
         Read-only — works with both full and read-only sandbox attachments."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &LS_SCHEMA
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
        let sandbox_id = subject
            .readable_sandbox_id()
            .ok_or(ToolSetsError::Unauthorized)?;
        let args = arguments.unwrap_or_default();

        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolSetsError::MissingArgument("path".to_string()))?;

        let client = self
            .sandboxes
            .instance_client_for(sandbox_id)
            .await
            .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;

        let req = ExecuteRequest {
            tool: "str_replace_based_edit_tool".to_string(),
            input: serde_json::json!({
                "command": "view",
                "path": path,
            }),
        };

        match client.execute(&req).await {
            Ok(resp) => {
                let output = if let Some(ignore_list) =
                    args.get("ignore").and_then(|v| v.as_array())
                {
                    // Filter out entries matching the ignore list
                    let ignore: Vec<&str> = ignore_list.iter().filter_map(|v| v.as_str()).collect();
                    resp.output
                        .lines()
                        .filter(|line| {
                            let name = line.trim_end_matches('/');
                            !ignore.contains(&name)
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    resp.output
                };

                let content = vec![Content::text(output)];
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
