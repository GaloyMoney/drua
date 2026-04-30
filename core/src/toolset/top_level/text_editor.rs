//! `str_replace_based_edit_tool` — the in-sandbox file editor. Wire-compatible
//! with Anthropic's built-in `text_editor_20250728`: same name, same
//! `command` discriminator (`view` / `create` / `str_replace` / `insert`),
//! same field names. Forwards the request body verbatim to the
//! sandbox-tool-server's `/execute` endpoint.
//!
//! Visibility / authz mirrors [`super::Bash`]:
//! - Visible only to [`AuthSubject::Agent`] / [`AuthSubject::AgentOnBehalfOfUser`].
//! - `view` is read-only → executable with either `SandboxUse` *or*
//!   `SandboxRead`.
//! - `create` / `str_replace` / `insert` mutate state → require
//!   `SandboxUse`. Enforced inside [`TextEditor::call`] after parsing
//!   the command, so the tool is visible-but-unauthorized for read-only
//!   attachments calling a write command (clear error, model can ask to
//!   upgrade the attachment).

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use sandbox::instance_client::ExecuteRequest;

use crate::audit::Audit;
use crate::auth::AuthSubject;
use crate::primitives::{AuthScope, SandboxId};
use crate::sandbox::Sandboxes;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{schema_for, TextOutput};

pub struct TextEditor {
    sandboxes: Arc<Sandboxes>,
}

impl TextEditor {
    pub fn new(sandboxes: Arc<Sandboxes>) -> Self {
        Self { sandboxes }
    }
}

static TEXT_EDITOR_OUTPUT_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<TextOutput>);

static TEXT_EDITOR_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    // Union of fields across all commands; `command` discriminates and
    // the server validates per-command requirements.
    serde_json::json!({
    "type": "object",
    "properties": {
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

fn writable_sandbox_id(subject: &AuthSubject) -> Option<SandboxId> {
    subject.scopes().iter().find_map(|s| match s {
        AuthScope::SandboxUse(id) => Some(*id),
        _ => None,
    })
}

/// `view` is the only read-only operation; everything else mutates.
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

    fn output_schema(&self) -> Option<&serde_json::Value> {
        Some(&TEXT_EDITOR_OUTPUT_SCHEMA)
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        // Hidden from project admins — see bash.rs.
        subject.is_agent() && !subject.is_project_admin()
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

        // Mutating commands require SandboxUse; `view` falls back to SandboxRead.
        let sandbox_id = if command_is_mutating(command) {
            writable_sandbox_id(subject).ok_or(ToolSetsError::Unauthorized)?
        } else {
            subject
                .readable_sandbox_id()
                .ok_or(ToolSetsError::Unauthorized)?
        };
        Audit::record_action("text_editor");
        Audit::record_sandbox_id(sandbox_id);

        let client = if command_is_mutating(command) {
            self.sandboxes
                .instance_client_for(subject, sandbox_id)
                .await
        } else {
            self.sandboxes
                .instance_client_for_read(subject, sandbox_id)
                .await
        }
        .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;

        let req = ExecuteRequest {
            tool: "str_replace_based_edit_tool".to_string(),
            input: serde_json::Value::Object(args),
        };

        match client.execute(&req).await {
            Ok(resp) => {
                let out = TextOutput {
                    output: resp.output,
                };
                let structured = serde_json::to_value(&out).expect("TextOutput serialization");
                let content = vec![Content::text(&out.output)];
                let mut result = if resp.is_error {
                    CallToolResult::error(content)
                } else {
                    CallToolResult::success(content)
                };
                result.structured_content = Some(structured);
                Ok(result)
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "sandbox /execute call failed: {e}"
            ))])),
        }
    }
}
