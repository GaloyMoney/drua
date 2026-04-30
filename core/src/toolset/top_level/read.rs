//! `Read` — read a file with optional line range inside the agent's attached
//! sandbox. Thin alias for the text editor's `view` command: translates
//! `{path, offset, limit}` into `{command: "view", path, view_range}` and
//! forwards to the same `/execute` handler.
//!
//! Read-only: executable with either `SandboxUse` or `SandboxRead`.

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use sandbox::instance_client::ExecuteRequest;
use serde::Deserialize;

use crate::audit::Audit;
use crate::auth::AuthSubject;
use crate::library::{parse_space_path, SpaceFs};
use crate::sandbox::Sandboxes;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{parse_params, schema_for, ContentOutput};

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
}

static READ_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<ReadParams>);

static READ_OUTPUT_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<ContentOutput>);

#[async_trait::async_trait]
impl TopLevelTool for Read {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        "Read a file with optional line range inside the agent's attached sandbox. \
         Read-only — works with both full and read-only sandbox attachments."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &READ_SCHEMA
    }

    fn output_schema(&self) -> Option<&serde_json::Value> {
        Some(&READ_OUTPUT_SCHEMA)
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        // See bash.rs.
        subject.is_agent() && !subject.is_project_admin()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: ReadParams = parse_params(arguments)?;
        Audit::record_action("read");

        // `space:<slug>/...` paths bypass the sandbox entirely.
        if let Some(sref) = parse_space_path(&params.path) {
            let view_range = view_range_from_offset_limit(params.offset, params.limit);
            return space_view_to_result(self.space_fs.view_file(subject, sref, view_range).await);
        }

        let sandbox_id = subject
            .readable_sandbox_id()
            .ok_or(ToolSetsError::Unauthorized)?;
        Audit::record_sandbox_id(sandbox_id);

        // Translate offset/limit into the editor's view_range; start is 1-based, -1 = EOF.
        let mut editor_input = serde_json::json!({
            "command": "view",
            "path": params.path,
        });

        if let Some((start, end)) = view_range_from_offset_limit(params.offset, params.limit) {
            editor_input["view_range"] = serde_json::json!([start, end]);
        }

        let client = self
            .sandboxes
            .instance_client_for_read(subject, sandbox_id)
            .await
            .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;

        let req = ExecuteRequest {
            tool: "str_replace_based_edit_tool".to_string(),
            input: editor_input,
        };

        match client.execute(&req).await {
            Ok(resp) => {
                let out = ContentOutput {
                    content: resp.output,
                };
                let structured = serde_json::to_value(&out).expect("ContentOutput serialization");
                let content = vec![Content::text(&out.content)];
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

fn space_view_to_result(
    res: Result<String, crate::project::ProjectError>,
) -> Result<CallToolResult, ToolSetsError> {
    match res {
        Ok(content) => {
            let out = ContentOutput {
                content: content.clone(),
            };
            let structured = serde_json::to_value(&out).expect("ContentOutput serialization");
            let mut result = CallToolResult::success(vec![Content::text(content)]);
            result.structured_content = Some(structured);
            Ok(result)
        }
        Err(e) => Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
    }
}
