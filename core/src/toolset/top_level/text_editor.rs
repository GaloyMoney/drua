//! `str_replace_based_edit_tool` — Anthropic-compatible text editor.
//! Two backends:
//!
//! - `space:<slug>/...` paths: all four commands route through
//!   `SpaceFs`. Reads (`view`) hit the on-disk library clone; writes
//!   (`create` / `str_replace` / `insert`) enqueue an `UpstreamOp` on
//!   the library lock queue and block until the upstream push lands.
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
use sandbox::{TextEditorAction, TextEditorInput};

use crate::audit::Audit;
use crate::auth::AuthSubject;
use crate::primitives::{AuthScope, SandboxId};
use crate::sandbox::Sandboxes;
use crate::space_fs::{FileView, SpaceFs};

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{parse_params, schema_for, OutputSchema, TextOutput};

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
static TEXT_EDITOR_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<TextEditorInput>);

fn writable_sandbox_id(subject: &AuthSubject) -> Option<SandboxId> {
    subject.scopes().iter().find_map(|s| match s {
        AuthScope::SandboxUse(id) => Some(*id),
        _ => None,
    })
}

#[async_trait::async_trait]
impl TopLevelTool for TextEditor {
    fn name(&self) -> &str {
        "Edit"
    }

    fn description(&self) -> &str {
        "Anthropic-compatible text editor. Commands: `view` (read file or list \
         directory), `create` (write a new file), `str_replace` (replace a \
         unique substring), `insert` (insert text at a line). Accepts both \
         in-sandbox absolute paths and `space:<slug>/...` paths from mounted \
         spaces — writes to spaces commit to the upstream library. \
         Notes for `str_replace`: `old_str` must match the file byte-for-byte \
         AND appear exactly once. If you don't already have the file content \
         in context, `view` it first — guessing the surrounding text will \
         fail. Notes for `create`: the path must point at a file, not a \
         directory; to add a folder, create a file inside it (e.g. \
         `<dir>/README.md`)."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &TEXT_EDITOR_SCHEMA
    }

    fn output_schema(&self) -> Option<&serde_json::Value> {
        Some(TEXT_EDITOR_OUTPUT.schema())
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.can_use_agent_file_tools()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let input: TextEditorInput = parse_params(arguments)?;
        Audit::record_action("text_editor");

        let is_mutating = input.is_mutating();

        if SpaceFs::is_space_path(&input.path) {
            let action = input
                .clone()
                .resolve()
                .map_err(|e| ToolSetsError::InvalidArgument(e.to_string()))?;
            let space_result: Option<String> = match action {
                TextEditorAction::View { path, view_range } => {
                    let range = view_range.map(|[s, e]| (s, e));
                    let number_range = view_range.map(|[s, e]| {
                        let start = s.max(1) as usize;
                        let end = if e == -1 { usize::MAX } else { e as usize };
                        (start, end)
                    });
                    self.space_fs
                        .view_file(subject, &path, range)
                        .await?
                        .map(|view| match view {
                            FileView::File(text) => sandbox::number_lines(&text, number_range),
                            FileView::Dir(entries) => entries.join("\n"),
                        })
                }
                TextEditorAction::Create { path, file_text } => self
                    .space_fs
                    .write_file(subject, &path, file_text)
                    .await?
                    .map(|()| format!("Wrote {path}")),
                TextEditorAction::StrReplace {
                    path,
                    old_str,
                    new_str,
                } => self
                    .space_fs
                    .str_replace(subject, &path, old_str, new_str)
                    .await?
                    .map(|()| format!("Replaced text in {path}")),
                TextEditorAction::Insert {
                    path,
                    insert_line,
                    new_str,
                } => self
                    .space_fs
                    .insert_line(subject, &path, insert_line as usize, new_str)
                    .await?
                    .map(|()| format!("Inserted text at line {insert_line} of {path}")),
            };
            if let Some(output) = space_result {
                let out = TextOutput {
                    output: output.clone(),
                };
                return Ok(TEXT_EDITOR_OUTPUT.success(output, &out));
            }
        }

        let sandbox_id = if is_mutating {
            writable_sandbox_id(subject).ok_or(ToolSetsError::Unauthorized)?
        } else {
            subject
                .readable_sandbox_id()
                .ok_or(ToolSetsError::Unauthorized)?
        };
        Audit::record_sandbox_id(sandbox_id);

        let client = if is_mutating {
            self.sandboxes
                .instance_client_for(subject, sandbox_id)
                .await
        } else {
            self.sandboxes
                .instance_client_for_read(subject, sandbox_id)
                .await
        }?;

        let view_range = match input.clone().resolve() {
            Ok(TextEditorAction::View { view_range, .. }) => view_range.map(|[s, e]| {
                let start = s.max(1) as usize;
                let end = if e == -1 { usize::MAX } else { e as usize };
                (start, end)
            }),
            _ => None,
        };
        let is_view = matches!(input.clone().resolve(), Ok(TextEditorAction::View { .. }));

        match client.execute_text_editor(&input).await {
            Ok(resp) => {
                let formatted = if is_view && !resp.is_error {
                    sandbox::number_lines(&resp.output, view_range)
                } else {
                    resp.output
                };
                let out = TextOutput {
                    output: formatted.clone(),
                };
                Ok(if resp.is_error {
                    TEXT_EDITOR_OUTPUT.error(formatted, &out)
                } else {
                    TEXT_EDITOR_OUTPUT.success(formatted, &out)
                })
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "sandbox /execute call failed: {e}"
            ))])),
        }
    }
}
