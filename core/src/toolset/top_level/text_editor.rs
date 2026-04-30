//! `str_replace_based_edit_tool` — the in-sandbox file editor. Wire-compatible
//! with Anthropic's built-in `text_editor_20250728`: same name, same
//! `command` discriminator (`view` / `create` / `str_replace` / `insert`),
//! same field names. Forwards the request body verbatim to the
//! sandbox-tool-server's `/execute` endpoint.
//!
//! Argument parsing goes through
//! [`AnthropicAdapter`](crate::toolset::edit::AnthropicAdapter) (identity
//! passthrough for the canonical schema) so the command/auth split stays in
//! sync with the canonical [`EditOp`](crate::toolset::edit::EditOp) used by
//! every non-Anthropic provider adapter.
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
use crate::toolset::edit::{
    AnthropicAdapter, AnthropicTextEditorVersion, EditOp, ProviderEditAdapter,
};

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{schema_for, TextOutput};

pub struct TextEditor {
    sandboxes: Arc<Sandboxes>,
    adapter: AnthropicAdapter,
}

impl TextEditor {
    pub fn new(sandboxes: Arc<Sandboxes>) -> Self {
        Self {
            sandboxes,
            adapter: AnthropicAdapter::new(AnthropicTextEditorVersion::V20250728),
        }
    }
}

static TEXT_EDITOR_OUTPUT_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<TextOutput>);

static TEXT_EDITOR_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    crate::toolset::edit::edit_input_schema(AnthropicTextEditorVersion::V20250728)
});

const TOOL_NAME: &str = "str_replace_based_edit_tool";

fn writable_sandbox_id(subject: &AuthSubject) -> Option<SandboxId> {
    subject.scopes().iter().find_map(|s| match s {
        AuthScope::SandboxUse(id) => Some(*id),
        _ => None,
    })
}

#[async_trait::async_trait]
impl TopLevelTool for TextEditor {
    fn name(&self) -> &str {
        TOOL_NAME
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
        // Hidden from workspace admins — see bash.rs.
        subject.is_agent() && !subject.is_workspace_admin()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let args_value = serde_json::Value::Object(arguments.unwrap_or_default());
        let mut ops = self
            .adapter
            .parse_call(TOOL_NAME, &args_value)
            .map_err(|e| ToolSetsError::InvalidArgument(e.to_string()))?;
        let op = ops
            .pop()
            .ok_or_else(|| ToolSetsError::InvalidArgument("empty edit op batch".into()))?;

        // Mutating ops require SandboxUse; `view` falls back to SandboxRead.
        let sandbox_id = if op.is_read_only() {
            subject
                .readable_sandbox_id()
                .ok_or(ToolSetsError::Unauthorized)?
        } else {
            writable_sandbox_id(subject).ok_or(ToolSetsError::Unauthorized)?
        };
        Audit::record_action("text_editor");
        Audit::record_sandbox_id(sandbox_id);

        let client = if op.is_read_only() {
            self.sandboxes
                .instance_client_for_read(subject, sandbox_id)
                .await
        } else {
            self.sandboxes
                .instance_client_for(subject, sandbox_id)
                .await
        }
        .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;

        let req = ExecuteRequest {
            tool: TOOL_NAME.to_string(),
            input: edit_op_to_sandbox_input(&op),
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

/// The sandbox executor speaks the Anthropic `text_editor_*` wire format,
/// so the canonical [`EditOp`] serialises straight through. The two extra
/// canonical variants ([`EditOp::Delete`] / [`EditOp::Rename`]) cannot
/// reach this path — Anthropic's schema-less server tool can only emit the
/// four core commands, and [`AnthropicAdapter::parse_call`] enforces that.
fn edit_op_to_sandbox_input(op: &EditOp) -> serde_json::Value {
    serde_json::to_value(op).expect("EditOp is always serializable")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_view_to_sandbox_input() {
        let op = EditOp::View {
            path: "/w/a.rs".to_string(),
            view_range: Some([1, -1]),
        };
        let v = edit_op_to_sandbox_input(&op);
        assert_eq!(v["command"], "view");
        assert_eq!(v["path"], "/w/a.rs");
        assert_eq!(v["view_range"], serde_json::json!([1, -1]));
    }

    #[test]
    fn round_trip_str_replace_to_sandbox_input() {
        let op = EditOp::StrReplace {
            path: "/w/a.rs".to_string(),
            old_str: "let x = 1;".into(),
            new_str: "let x = 2;".into(),
        };
        let v = edit_op_to_sandbox_input(&op);
        assert_eq!(v["command"], "str_replace");
        assert_eq!(v["old_str"], "let x = 1;");
        assert_eq!(v["new_str"], "let x = 2;");
    }

    #[test]
    fn round_trip_insert_uses_new_str_field() {
        let op = EditOp::Insert {
            path: "/w/a.rs".to_string(),
            insert_line: 0,
            new_str: "header\n".into(),
        };
        let v = edit_op_to_sandbox_input(&op);
        assert_eq!(v["command"], "insert");
        assert_eq!(v["new_str"], "header\n");
        assert_eq!(v["insert_line"], 0);
    }
}
