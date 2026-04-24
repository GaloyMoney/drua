//! `Grep` — content search across files inside the agent's attached sandbox.
//! Wire-compatible with Claude Code's `Grep` tool and ripgrep-style flags.
//!
//! Read-only: executable with either `SandboxUse` or `SandboxRead`.
//! Server-side handler shells out to `rg`.

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use sandbox::instance_client::ExecuteRequest;

use crate::audit::Audit;
use crate::auth::AuthSubject;
use crate::sandbox::Sandboxes;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;

pub struct Grep {
    sandboxes: Arc<Sandboxes>,
}

impl Grep {
    pub fn new(sandboxes: Arc<Sandboxes>) -> Self {
        Self { sandboxes }
    }
}

static GREP_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "Regular expression pattern to search for in file contents."
            },
            "path": {
                "type": "string",
                "description": "File or directory to search in. Defaults to workspace root."
            },
            "glob": {
                "type": "string",
                "description": "Glob pattern to filter files (e.g. '*.rs', '**/*.{ts,tsx}')."
            },
            "type": {
                "type": "string",
                "description": "File type to search (e.g. 'rust', 'py', 'js')."
            },
            "output_mode": {
                "type": "string",
                "enum": ["files_with_matches", "content", "count"],
                "description": "Output mode. Default: 'files_with_matches'."
            },
            "-i": {
                "type": "boolean",
                "description": "Case insensitive search."
            },
            "-n": {
                "type": "boolean",
                "description": "Show line numbers (only with output_mode='content'). Default: true."
            },
            "-A": {
                "type": "integer",
                "description": "Number of lines to show after each match."
            },
            "-B": {
                "type": "integer",
                "description": "Number of lines to show before each match."
            },
            "-C": {
                "type": "integer",
                "description": "Number of context lines before and after each match."
            },
            "head_limit": {
                "type": "integer",
                "description": "Cap output to first N lines."
            },
            "multiline": {
                "type": "boolean",
                "description": "Enable multiline mode where . matches newlines."
            }
        },
        "required": ["pattern"],
        "additionalProperties": false,
    })
});

#[async_trait::async_trait]
impl TopLevelTool for Grep {
    fn name(&self) -> &str {
        "Grep"
    }

    fn description(&self) -> &str {
        "Search file contents using ripgrep inside the agent's attached sandbox. \
         Supports regex patterns, glob filters, file-type filters, and context \
         lines. Read-only — works with both full and read-only sandbox attachments."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &GREP_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        // See bash.rs: hidden from workspace admins.
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
        Audit::record_action("grep");
        Audit::record_sandbox_id(sandbox_id);

        let client = self
            .sandboxes
            .instance_client_for_read(subject, sandbox_id)
            .await
            .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;

        let req = ExecuteRequest {
            tool: "Grep".to_string(),
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
