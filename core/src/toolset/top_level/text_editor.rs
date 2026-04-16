//! `str_replace_based_edit_tool` — the in-sandbox file editor. Wire-compatible
//! with Anthropic's built-in `text_editor_20250728`: same name, same
//! `command` discriminator (`view` / `create` / `str_replace` / `insert`),
//! same field names. Forwards the request body verbatim to the
//! sandbox-tool-server's `/execute` endpoint.
//!
//! Visibility / authz mirrors [`super::Bash`]:
//! - Visible only to [`AuthSubject::Agent`] / [`AuthSubject::AgentOnBehalfOfUser`].
//! - `view` is read-only → executable with either `SandboxUseAll` *or*
//!   `SandboxUseReadOnly`.
//! - `create` / `str_replace` / `insert` mutate state → require
//!   `SandboxUseAll`. Enforced inside [`TextEditor::call`] after parsing
//!   the command, so the tool is visible-but-unauthorized for read-only
//!   attachments calling a write command (clear error, model can ask to
//!   upgrade the attachment).

use std::sync::LazyLock;

use rmcp::model::{CallToolResult, Content, JsonObject};
use sandbox::instance_client::ExecuteRequest;

use crate::auth::AuthSubject;
use crate::primitives::{AuthScope, SandboxId};
use crate::sandbox::Sandboxes;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;

pub struct TextEditor {
    sandboxes: Sandboxes,
}

impl TextEditor {
    pub fn new(sandboxes: Sandboxes) -> Self {
        Self { sandboxes }
    }
}

static TEXT_EDITOR_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    // Single object with the union of fields across all four commands.
    // The `command` discriminator selects which other fields are
    // meaningful — server validates the per-command requirements. The
    // model already knows the built-in's shape, so detailed per-command
    // schemas would be redundant.
    serde_json::json!({
            "type": "object",
    tep        "properties": {
                "command": {
                    "type": "string",
                    "enum": ["view", "create", "str_replace", "insert"],
                    "description": "Which editor operation to perform."
                },
                "path": {
                    "type": "string",
                    "description": "Absolute path to the file or directory inside the sandbox workspace."
                },
                "view_range": {
                    "type": "array",
                    "items": { "type": "integer" },
                    "minItems": 2,
                    "maxItems": 2,
                    "description": "[start, end] line range for `view`. -1 for end means EOF. Optional."
                },
                "file_text": {
                    "type": "string",
                    "description": "Full file contents for `create`."
                },
                "old_str": {
                    "type": "string",
                    "description": "Substring to replace for `str_replace`. Must appear exactly once."
                },
                "new_str": {
                    "type": "string",
                    "description": "Replacement text for `str_replace`, or text to insert for `insert`."
                },
                "insert_line": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Line number after which to insert text for `insert`. 0 = beginning of file."
                }
            },
            "required": ["command", "path"],
            "additionalProperties": false,
        })
});

/// First sandbox the subject can write to (full UseAll attach).
fn writable_sandbox_id(subject: &AuthSubject) -> Option<SandboxId> {
    subject.scopes().iter().find_map(|s| match s {
        AuthScope::SandboxUseAll(id) => Some(*id),
        _ => None,
    })
}

/// Whether `command` writes to the filesystem. `view` is the only
/// read-only operation; everything else mutates a file.
fn command_is_mutating(command: &str) -> bool {
    !matches!(command, "view")
}

#[async_trait::async_trait]
impl TopLevelTool for TextEditor {
    fn name(&self) -> &str {
        "str_replace_based_edit_tool"
    }

    fn description(&self) -> &str {
        "Anthropic-compatible text editor for the agent's attached sandbox. \
         Commands: `view` (read file or list directory), `create` (write a \
         new file), `str_replace` (replace a unique substring), `insert` \
         (insert text at a line). `view` works with read-only attachments; \
         the other commands require write access."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &TEXT_EDITOR_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        // See bash.rs: workspace admins don't run inside sandboxes, hide it.
        subject.is_agent() && !subject.is_workspace_admin()
    }

    fn can_execute(&self, subject: &AuthSubject) -> bool {
        // Permissive at dispatch — allow if the subject can read *or*
        // write any sandbox. Per-command authz happens inside `call()`
        // once we know whether the request is mutating.
        subject.is_agent() && subject.readable_sandbox_id().is_some()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let args = arguments.unwrap_or_default();
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolSetsError::MissingArgument("command".to_string()))?;

        // Resolve target sandbox + per-command authz.  Mutating commands
        // require SandboxUseAll; `view` falls back to SandboxUseReadOnly.
        let sandbox_id = if command_is_mutating(command) {
            writable_sandbox_id(subject).ok_or(ToolSetsError::Unauthorized)?
        } else {
            subject
                .readable_sandbox_id()
                .ok_or(ToolSetsError::Unauthorized)?
        };

        let client = self
            .sandboxes
            .instance_client_for(sandbox_id)
            .await
            .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;

        let req = ExecuteRequest {
            tool: "str_replace_based_edit_tool".to_string(),
            input: serde_json::Value::Object(args),
        };

        // Forward `is_error` as-is so the model sees the same shape it
        // gets from Anthropic's built-in editor.
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
