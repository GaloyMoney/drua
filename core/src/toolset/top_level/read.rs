//! `Read` — read a file with optional line range. Two backends:
//!
//! - `space:<slug>/...` paths route through `SpaceFs` (no sandbox needed).
//! - Anything else translates `{path, offset, limit}` into the text
//!   editor's `view` command and forwards to the agent's attached
//!   sandbox via `/execute`.
//!
//! Read-only: executable with either `SandboxUse` or `SandboxRead`.
//!
//! Line numbers are presentation for a model reading a tool result, not
//! data — a compose script is a data consumer. `call` (the MCP path)
//! numbers; `call_from_script` (compose scripts, including
//! `workflow:script_step`) returns the exact text instead: byte-exact for
//! whole-file reads, `\n`-joined line slices for ranged reads (see
//! `space_fs::apply_view_range`).
//!
//! The same split gates `SpaceFs`'s `MAX_VIEW_FILE_BYTES` cap: a model
//! read stays capped at 1 MiB, a script read is uncapped (bounded only
//! by `toolsets.compose.max_tool_result_bytes`) — see
//! `SpaceFs::view_file_with_cap`.

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use sandbox::instance_client::ExecuteRequest;
use serde::Deserialize;

use crate::audit::Audit;
use crate::auth::AuthSubject;
use crate::sandbox::Sandboxes;
use crate::space_fs::{FileView, SpaceFs};

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{parse_params, schema_for, ContentOutput, OutputSchema};

#[derive(Deserialize, schemars::JsonSchema)]
struct ReadParams {
    path: String,
    #[serde(default, deserialize_with = "super::liberal::deserialize_option_i64")]
    offset: Option<i64>,
    #[serde(default, deserialize_with = "super::liberal::deserialize_option_i64")]
    limit: Option<i64>,
}

pub struct Read {
    sandboxes: Arc<Sandboxes>,
    space_fs: Arc<SpaceFs>,
}

impl Read {
    pub fn new(sandboxes: Arc<Sandboxes>, space_fs: Arc<SpaceFs>) -> Self {
        Self {
            sandboxes,
            space_fs,
        }
    }

    /// Shared body for `call`/`call_from_script`. `for_model` is one
    /// decision made twice: line-numbered + capped at
    /// `MAX_VIEW_FILE_BYTES` for a model, exact text + uncapped (bounded
    /// only by `toolsets.compose.max_tool_result_bytes`) for a script.
    async fn read(
        &self,
        subject: &AuthSubject,
        params: ReadParams,
        for_model: bool,
    ) -> Result<CallToolResult, ToolSetsError> {
        Audit::record_action("read");

        let view_range = view_range_from_offset_limit(params.offset, params.limit);
        let space_view = if for_model {
            self.space_fs
                .view_file(subject, &params.path, view_range)
                .await?
        } else {
            self.space_fs
                .view_file_with_cap(subject, &params.path, view_range, None)
                .await?
        };

        if let Some(view) = space_view {
            let content = render_file_view(view, for_model, view_range);
            let out = ContentOutput {
                content: content.clone(),
            };
            return Ok(READ_OUTPUT.success(content, &out));
        }

        let sandbox_id = subject
            .readable_sandbox_id()
            .ok_or_else(|| super::sandbox_read_denied("Read"))?;
        Audit::record_sandbox_id(sandbox_id);

        let mut editor_input = serde_json::json!({
            "command": "view",
            "path": params.path,
        });

        if let Some((start, end)) = view_range {
            editor_input["view_range"] = serde_json::json!([start, end]);
        }

        let client = self
            .sandboxes
            .instance_client_for_read(subject, sandbox_id)
            .await?;

        let req = ExecuteRequest {
            tool: "str_replace_based_edit_tool".to_string(),
            input: editor_input,
        };

        match client.execute(&req).await {
            Ok(resp) => {
                let content = if resp.is_error || !for_model {
                    resp.output
                } else {
                    sandbox::number_lines(&resp.output, numbering_range(view_range))
                };
                let out = ContentOutput {
                    content: content.clone(),
                };
                Ok(if resp.is_error {
                    READ_OUTPUT.error(content, &out)
                } else {
                    READ_OUTPUT.success(content, &out)
                })
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "sandbox /execute call failed: {e}"
            ))])),
        }
    }
}

static READ_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<ReadParams>);
static READ_OUTPUT: LazyLock<OutputSchema<ContentOutput>> = LazyLock::new(OutputSchema::new);

#[async_trait::async_trait]
impl TopLevelTool for Read {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        "Read a file with optional line range. Accepts either an in-sandbox path \
         or a `space:<slug>/...` path that reads from the project's mounted spaces. \
         Inside compose scripts the result is the exact file text without line \
         numbers (whole-file reads are byte-exact; ranged reads are `\\n`-joined \
         line slices), and a `space:` read is not subject to the 1 MiB cap this \
         tool otherwise applies."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &READ_SCHEMA
    }

    fn inner_output_schema(&self) -> Option<&serde_json::Value> {
        Some(READ_OUTPUT.schema())
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.can_use_agent_file_tools()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: ReadParams = parse_params(arguments)?;
        self.read(subject, params, true).await
    }

    async fn call_from_script(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: ReadParams = parse_params(arguments)?;
        self.read(subject, params, false).await
    }
}

/// `[start, end]` (1-based, `-1` = EOF) when either bound is supplied;
/// `None` when both are unset (full file).
fn view_range_from_offset_limit(offset: Option<i64>, limit: Option<i64>) -> Option<(i64, i64)> {
    if offset.is_none() && limit.is_none() {
        return None;
    }
    let start = offset.unwrap_or(0) + 1;
    let end = match limit {
        Some(l) => start + l - 1,
        None => -1,
    };
    Some((start, end))
}

/// `sandbox::number_lines` wants a `(usize, usize)` window; `view_range`
/// is the 1-based, `-1`-terminated `(i64, i64)` shape shared with the
/// space and sandbox backends.
fn numbering_range(view_range: Option<(i64, i64)>) -> Option<(usize, usize)> {
    view_range.map(|(s, e)| {
        let start = s.max(1) as usize;
        let end = if e == -1 { usize::MAX } else { e as usize };
        (start, end)
    })
}

/// A `space:` file/dir view, presented for a model (`number: true`,
/// line-numbered) or a compose script (`number: false`, exact text —
/// byte-exact for a whole-file read, `\n`-joined for a ranged one).
/// Directory listings are the same either way: numbering is moot for them.
fn render_file_view(view: FileView, number: bool, view_range: Option<(i64, i64)>) -> String {
    match view {
        FileView::File(text) if number => sandbox::number_lines(&text, numbering_range(view_range)),
        FileView::File(text) => text,
        FileView::Dir(entries) => entries.join("\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_range_none_when_offset_and_limit_unset() {
        assert_eq!(view_range_from_offset_limit(None, None), None);
    }

    #[test]
    fn view_range_from_offset_and_limit() {
        assert_eq!(
            view_range_from_offset_limit(Some(10), Some(5)),
            Some((11, 15))
        );
    }

    #[test]
    fn view_range_open_ended_without_limit() {
        assert_eq!(view_range_from_offset_limit(Some(10), None), Some((11, -1)));
    }

    #[test]
    fn render_file_view_numbers_for_a_model() {
        let out = render_file_view(FileView::File("a\nb".into()), true, None);
        assert!(out.starts_with("     1\ta"), "{out}");
    }

    #[test]
    fn render_file_view_is_exact_for_a_script() {
        let out = render_file_view(FileView::File("a\r\nb\r\n".into()), false, None);
        assert_eq!(out, "a\r\nb\r\n");
    }

    #[test]
    fn render_file_view_unnumbered_ignores_view_range() {
        // The range slice already happened upstream, inside
        // `SpaceFs::view_file` (`apply_view_range`) — that's also where
        // a ranged read loses `\r`/the trailing newline, for both a
        // model and a script. `render_file_view` only ever decides
        // whether to number an already-resolved `FileView`, so a
        // `view_range` here affects numbering, never the text itself.
        let out = render_file_view(FileView::File("a\r\nb\r\n".into()), false, Some((1, 1)));
        assert_eq!(out, "a\r\nb\r\n");
    }

    #[test]
    fn render_file_view_dir_ignores_number_flag() {
        let dir = FileView::Dir(vec!["a.md".into(), "b/".into()]);
        assert_eq!(render_file_view(dir, true, None), "a.md\nb/");
    }

    #[test]
    fn schema_has_no_raw_property() {
        assert!(
            READ_SCHEMA["properties"].get("raw").is_none(),
            "raw was removed with Rev 2 — the switch is call vs call_from_script, not a parameter"
        );
    }
}
