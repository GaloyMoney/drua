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
use sandbox::instance_client::ExecuteRequest;

use crate::audit::Audit;
use crate::auth::AuthSubject;
use crate::primitives::{AuthScope, SandboxId};
use crate::sandbox::Sandboxes;
use crate::space_fs::{BatchedSpaceWrite, BatchedSpaceWriteKind, FileView, SpaceFs};

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
                "description": "Substring to replace for `str_replace`. Must match the file byte-for-byte and appear exactly once. `view` the file first if you don't have its current content in context."
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

/// Builds a helpful `MissingArgument` error that names the missing
/// field and shows the full per-command recipe inline. Streaming
/// model glitches sometimes produce empty `{}` tool calls; a concrete
/// example in the error body lets the next turn self-correct without
/// a follow-up "what should I send?" round-trip.
fn missing_arg(field: &str) -> ToolSetsError {
    let recipe = match field {
        "command" => {
            "Required: { command: \"view\"|\"create\"|\"str_replace\"|\"insert\", path: \"...\", \
             [view_range|file_text|old_str|new_str|insert_line per command] }"
        }
        "file_text" => {
            "create requires: { command: \"create\", path: \"...\", file_text: \"...\" }"
        }
        "old_str" | "new_str" => {
            "str_replace requires: { command: \"str_replace\", path: \"...\", old_str: \"...\", \
             new_str: \"...\" }"
        }
        "insert_line" => {
            "insert requires: { command: \"insert\", path: \"...\", insert_line: <int>, \
             new_str: \"...\" }"
        }
        _ => "see tool input_schema for required fields",
    };
    ToolSetsError::MissingArgument(format!("{field}. {recipe}"))
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
         Independent Edit calls in one turn commit atomically. \
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
        let args = arguments.unwrap_or_default();
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| missing_arg("command"))?
            .to_string();
        Audit::record_action("text_editor");

        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");

        // `space:<slug>/...` paths are handled entirely by SpaceFs —
        // both reads (`view`) and writes (`create` / `str_replace` /
        // `insert`). Each helper returns Ok(None) on non-space paths
        // so we fall through to the sandbox dispatch below.
        if SpaceFs::is_space_path(path) {
            let space_result: Option<String> = match command.as_str() {
                "view" => {
                    let view_range =
                        args.get("view_range")
                            .and_then(|v| v.as_array())
                            .and_then(|arr| {
                                let s = arr.first()?.as_i64()?;
                                let e = arr.get(1)?.as_i64()?;
                                Some((s, e))
                            });
                    self.space_fs
                        .view_file(subject, path, view_range)
                        .await?
                        .map(|view| match view {
                            FileView::File(text) => text,
                            FileView::Dir(entries) => entries.join("\n"),
                        })
                }
                "create" => {
                    let file_text = args
                        .get("file_text")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| missing_arg("file_text"))?
                        .to_string();
                    self.space_fs
                        .write_file(subject, path, file_text)
                        .await?
                        .map(|()| format!("Wrote {path}"))
                }
                "str_replace" => {
                    let old_str = args
                        .get("old_str")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| missing_arg("old_str"))?
                        .to_string();
                    let new_str = args
                        .get("new_str")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| missing_arg("new_str"))?
                        .to_string();
                    self.space_fs
                        .str_replace(subject, path, old_str, new_str)
                        .await?
                        .map(|()| format!("Replaced text in {path}"))
                }
                "insert" => {
                    let line_number = args
                        .get("insert_line")
                        .and_then(|v| v.as_i64())
                        .ok_or_else(|| missing_arg("insert_line"))?
                        .max(0) as usize;
                    let new_str = args
                        .get("new_str")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| missing_arg("new_str"))?
                        .to_string();
                    self.space_fs
                        .insert_line(subject, path, line_number, new_str)
                        .await?
                        .map(|()| format!("Inserted text at line {line_number} of {path}"))
                }
                _ => {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "unknown text_editor command: {command}"
                    ))]));
                }
            };
            if let Some(output) = space_result {
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
        }?;

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

    /// Inputs the dispatcher routes here are guaranteed (by
    /// [`Self::batch_key`]) to be space-write commands. Coalesce them
    /// into a single git commit via `space_fs.apply_batch`. Reads /
    /// sandbox-bound calls return `None` from `batch_key` and never
    /// reach this path, so a write failure can never block them.
    async fn call_batch(
        &self,
        subject: &AuthSubject,
        inputs: Vec<Option<JsonObject>>,
    ) -> Vec<Result<CallToolResult, ToolSetsError>> {
        let mut writes: Vec<BatchedSpaceWrite> = Vec::with_capacity(inputs.len());
        for args in &inputs {
            // `batch_key` already vetted these; `parse_space_write` should
            // always return Some here. If it doesn't (model emitted
            // arguments that pass batch_key's superficial check but fail
            // here), fall through to per-call dispatch for that input.
            if let Some(w) = parse_space_write(args.as_ref()) {
                writes.push(w);
            } else {
                writes.push(BatchedSpaceWrite {
                    path: String::new(),
                    kind: BatchedSpaceWriteKind::Delete,
                });
            }
        }

        let outcomes = self.space_fs.apply_batch(subject, writes.clone()).await;

        outcomes
            .into_iter()
            .zip(writes)
            .map(|(outcome, w)| match outcome {
                Ok(Some(())) => {
                    let summary = format_success(&w);
                    let out = TextOutput {
                        output: summary.clone(),
                    };
                    Ok(TEXT_EDITOR_OUTPUT.success(summary, &out))
                }
                Ok(None) => Ok(CallToolResult::error(vec![Content::text(format!(
                    "space path did not resolve: {}",
                    w.path
                ))])),
                Err(e) => Err(ToolSetsError::from(e)),
            })
            .collect()
    }

    /// Group all space-write commands into one batch; everything else
    /// (view, sandbox paths) goes through `call`. The dispatcher uses
    /// the returned key to coalesce calls — same key in one turn = one
    /// git commit.
    fn batch_key(&self, arguments: Option<&JsonObject>) -> Option<&'static str> {
        parse_space_write(arguments).map(|_| "Edit:space_write")
    }
}

fn format_success(w: &BatchedSpaceWrite) -> String {
    match &w.kind {
        BatchedSpaceWriteKind::Write { .. } => format!("Wrote {}", w.path),
        BatchedSpaceWriteKind::Delete => format!("Deleted {}", w.path),
        BatchedSpaceWriteKind::StrReplace { .. } => format!("Replaced text in {}", w.path),
        BatchedSpaceWriteKind::Insert { line_number, .. } => {
            format!("Inserted text at line {} of {}", line_number, w.path)
        }
        // Unreachable: text_editor never produces Move kinds (Move is its own tool).
        BatchedSpaceWriteKind::Move { to_path } => format!("Moved {} -> {to_path}", w.path),
    }
}

/// Returns `Some(BatchedSpaceWrite)` iff `arguments` is a well-formed
/// space-write tool_use (create / str_replace / insert against a
/// `space:<slug>/...` path with all required fields). Anything else
/// returns `None` so the dispatcher falls through to per-call `call`.
fn parse_space_write(arguments: Option<&JsonObject>) -> Option<BatchedSpaceWrite> {
    let args = arguments?;
    let command = args.get("command")?.as_str()?;
    let path = args.get("path")?.as_str()?;
    if !SpaceFs::is_space_path(path) {
        return None;
    }
    let kind = match command {
        "create" => BatchedSpaceWriteKind::Write {
            content: args.get("file_text")?.as_str()?.to_string(),
        },
        "str_replace" => BatchedSpaceWriteKind::StrReplace {
            old_str: args.get("old_str")?.as_str()?.to_string(),
            new_str: args.get("new_str")?.as_str()?.to_string(),
        },
        "insert" => BatchedSpaceWriteKind::Insert {
            line_number: args.get("insert_line")?.as_i64()?.max(0) as usize,
            text: args.get("new_str")?.as_str()?.to_string(),
        },
        _ => return None,
    };
    Some(BatchedSpaceWrite {
        path: path.to_string(),
        kind,
    })
}
