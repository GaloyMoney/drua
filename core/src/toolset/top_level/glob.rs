//! `Glob` — file pattern matching inside the agent's attached sandbox.
//! Returns matching file paths sorted by modification time (most recent first).
//!
//! Read-only: executable with either `SandboxUseAll` or `SandboxUseReadOnly`.
//! Server-side handler uses `rg --files -g <pattern>`.

use std::sync::LazyLock;

use rmcp::model::{CallToolResult, Content, JsonObject};
use sandbox::instance_client::ExecuteRequest;

use crate::auth::AuthSubject;
use crate::sandbox::Sandboxes;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;

pub struct GlobTool {
    sandboxes: Sandboxes,
}

impl GlobTool {
    pub fn new(sandboxes: Sandboxes) -> Self {
        Self { sandboxes }
    }
}

static GLOB_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "Glob pattern to match files (e.g. '**/*.rs', 'src/**/*.ts')."
            },
            "path": {
                "type": "string",
                "description": "Directory to search in. Defaults to workspace root."
            }
        },
        "required": ["pattern"],
        "additionalProperties": false,
    })
});

#[async_trait::async_trait]
impl TopLevelTool for GlobTool {
    fn name(&self) -> &str {
        "Glob"
    }

    fn description(&self) -> &str {
        "Find files matching a glob pattern inside the agent's attached sandbox. \
         Returns matching file paths. Read-only — works with both full and \
         read-only sandbox attachments."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &GLOB_SCHEMA
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

        let client = self
            .sandboxes
            .instance_client_for(sandbox_id)
            .await
            .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;

        let req = ExecuteRequest {
            tool: "Glob".to_string(),
            input: serde_json::Value::Object(arguments.unwrap_or_default()),
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
