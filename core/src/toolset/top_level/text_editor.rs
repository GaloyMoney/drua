//! `str_replace_based_edit_tool` — Anthropic-compatible text editor.
//! Two backends:
//!
//! - `space:<slug>/...` paths: `view` runs through `SpaceFs::view_file`
//!   (no sandbox needed); mutating commands return a clean
//!   "not yet supported" error.
//! - Anything else: forwards verbatim to the agent's attached
//!   sandbox via the `/execute` endpoint.
//!
//! Visibility / authz mirrors [`super::Bash`]:
//! - Visible only to [`AuthSubject::Agent`] / [`AuthSubject::AgentOnBehalfOfUser`].
//! - `view` is read-only → executable with either `SandboxUse` *or*
//!   `SandboxRead`.
//! - `create` / `str_replace` / `insert` mutate state → require
//!   `SandboxUse`. Enforced inside [`TextEditor::call`] after parsing
//!   the command.

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use sandbox::instance_client::ExecuteRequest;

use crate::audit::Audit;
use crate::auth::AuthSubject;
use crate::library::{FileView, SpaceFs};
use crate::primitives::{AuthScope, SandboxId};
use crate::sandbox::Sandboxes;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{OutputSchema, TextOutput};

pub struct TextEditor {
    sandboxes: Arc<Sandboxes>,
    space_fs: Arc<SpaceFs>,
}

impl TextEditor {
    pub fn new(sandboxes: Arc<Sandboxes>, space_fs: Arc<SpaceFs>) -> Self {
        Self {
            sandboxes,
            space_fs,
        }
    }
}

static TEXT_EDITOR_OUTPUT: LazyLock<OutputSchema<TextOutput>> = LazyLock::new(OutputSchema::new);

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
                "description": "Path to the file or directory. In-sandbox absolute path, or `space:<slug>/...` for a mounted space (view-only)."
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
        "Edit"
    }

    fn description(&self) -> &str {
        "Anthropic-compatible text editor. Commands: `view` (read file or list \
         directory; supports `space:<slug>/...` paths from mounted spaces), \
         `create` / `str_replace` / `insert` (sandbox-only writes)."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &TEXT_EDITOR_SCHEMA
    }

    fn output_schema(&self) -> Option<&serde_json::Value> {
        Some(TEXT_EDITOR_OUTPUT.schema())
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
            .ok_or_else(|| ToolSetsError::MissingArgument("command".to_string()))?
            .to_string();
        Audit::record_action("text_editor");

        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");

        // Mutating commands on `space:` paths short-circuit before
        // hitting `SpaceFs` so we don't run the auth gate just to
        // return a "not supported" message.
        if command_is_mutating(&command) && SpaceFs::is_space_path(path) {
            return Ok(CallToolResult::error(vec![Content::text(
                "write commands on space paths are not yet supported. \
                 For now, use the spaces tool to create files in spaces."
                    .to_string(),
            )]));
        }

        // `view` against `space:<slug>/...` runs through SpaceFs. For
        // any other path SpaceFs returns Ok(None) and we fall through.
        if !command_is_mutating(&command) {
            let view_range = args
                .get("view_range")
                .and_then(|v| v.as_array())
                .and_then(|arr| {
                    let s = arr.first()?.as_i64()?;
                    let e = arr.get(1)?.as_i64()?;
                    Some((s, e))
                });
            let space_view = self
                .space_fs
                .view_file(subject, path, view_range)
                .await
                .map_err(|e| ToolSetsError::Project(e.to_string()))?;
            if let Some(view) = space_view {
                let output = match view {
                    FileView::File(text) => text,
                    FileView::Dir(entries) => entries.join("\n"),
                };
                let out = TextOutput {
                    output: output.clone(),
                };
                return Ok(TEXT_EDITOR_OUTPUT.success(output, &out));
            }
        }

        // Mutating commands require SandboxUse; `view` falls back to SandboxRead.
        let sandbox_id = if command_is_mutating(&command) {
            writable_sandbox_id(subject).ok_or(ToolSetsError::Unauthorized)?
        } else {
            subject
                .readable_sandbox_id()
                .ok_or(ToolSetsError::Unauthorized)?
        };
        Audit::record_sandbox_id(sandbox_id);

        let client = if command_is_mutating(&command) {
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
                    output: resp.output.clone(),
                };
                Ok(if resp.is_error {
                    TEXT_EDITOR_OUTPUT.error(resp.output, &out)
                } else {
                    TEXT_EDITOR_OUTPUT.success(resp.output, &out)
                })
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "sandbox /execute call failed: {e}"
            ))])),
        }
    }
}
