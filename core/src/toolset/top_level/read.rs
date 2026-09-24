//! `Read` — read a file with optional line range. Two backends:
//!
//! - `space:<slug>/...` paths route through `SpaceFs` (no sandbox needed).
//! - Anything else translates `{path, offset, limit}` into the text
//!   editor's `view` command and forwards to the agent's attached
//!   sandbox via `/execute`.
//!
//! Read-only: executable with either `SandboxUse` or `SandboxRead`.

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
    /// Exact file text, no line numbers. Whole file only.
    #[serde(default, deserialize_with = "super::liberal::deserialize_bool")]
    raw: bool,
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
         Pass raw: true to get the exact file text with no line numbers (whole file only)."
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
        Audit::record_action("read");

        validate_raw_range(params.raw, params.offset, params.limit)?;

        let view_range = view_range_from_offset_limit(params.offset, params.limit);
        let space_view = self
            .space_fs
            .view_file(subject, &params.path, view_range)
            .await?;

        if let Some(view) = space_view {
            let content = match view {
                FileView::File(text) => {
                    if params.raw {
                        text
                    } else {
                        let range = view_range.map(|(s, e)| {
                            let start = s.max(1) as usize;
                            let end = if e == -1 { usize::MAX } else { e as usize };
                            (start, end)
                        });
                        sandbox::number_lines(&text, range)
                    }
                }
                FileView::Dir(_) if params.raw => {
                    return Err(ToolSetsError::InvalidArgument(
                        "raw reads require a file path".into(),
                    ));
                }
                FileView::Dir(entries) => entries.join("\n"),
            };
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
                let content = if resp.is_error || params.raw {
                    resp.output
                } else {
                    let range = view_range.map(|(s, e)| {
                        let start = s.max(1) as usize;
                        let end = if e == -1 { usize::MAX } else { e as usize };
                        (start, end)
                    });
                    sandbox::number_lines(&resp.output, range)
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

/// `raw: true` returns the whole file verbatim, so it can't be combined
/// with a line range — there's no CRLF/final-newline convention for a
/// ranged raw read.
fn validate_raw_range(
    raw: bool,
    offset: Option<i64>,
    limit: Option<i64>,
) -> Result<(), ToolSetsError> {
    if raw && (offset.is_some() || limit.is_some()) {
        return Err(ToolSetsError::InvalidArgument(
            "raw reads return the whole file; omit offset/limit".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_with_offset_is_rejected() {
        let err = validate_raw_range(true, Some(0), None).unwrap_err();
        assert!(matches!(err, ToolSetsError::InvalidArgument(_)));
        assert_eq!(
            err.to_string(),
            "ToolSetsError - InvalidArgument: raw reads return the whole file; omit offset/limit"
        );
    }

    #[test]
    fn raw_with_limit_is_rejected() {
        assert!(validate_raw_range(true, None, Some(10)).is_err());
    }

    #[test]
    fn raw_without_range_is_accepted() {
        assert!(validate_raw_range(true, None, None).is_ok());
    }

    #[test]
    fn non_raw_with_range_is_accepted() {
        assert!(validate_raw_range(false, Some(0), Some(10)).is_ok());
    }

    #[test]
    fn raw_defaults_to_false() {
        let params: ReadParams = serde_json::from_value(serde_json::json!({
            "path": "space:demo/foo.md",
        }))
        .unwrap();
        assert!(!params.raw);
    }

    #[test]
    fn raw_accepts_json_bool() {
        let params: ReadParams = serde_json::from_value(serde_json::json!({
            "path": "space:demo/foo.md",
            "raw": true,
        }))
        .unwrap();
        assert!(params.raw);
    }

    #[test]
    fn raw_accepts_string_bool_liberal_deserializer() {
        let params: ReadParams = serde_json::from_value(serde_json::json!({
            "path": "space:demo/foo.md",
            "raw": "true",
        }))
        .unwrap();
        assert!(params.raw);
    }

    #[test]
    fn schema_exposes_raw_as_optional_boolean() {
        let raw_schema = READ_SCHEMA["properties"]["raw"]
            .as_object()
            .expect("raw schema should be present");
        assert_eq!(raw_schema["type"], "boolean");
        assert!(
            !READ_SCHEMA["required"]
                .as_array()
                .expect("required array should be present")
                .iter()
                .any(|v| v == "raw"),
            "raw should not be required"
        );
    }
}
