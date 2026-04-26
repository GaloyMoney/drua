//! `LS` — directory listing inside the agent's attached sandbox.
//! Thin alias for the text editor's `view` command on a directory path:
//! translates `{path, ignore}` into `{command: "view", path}` and
//! forwards to the same `/execute` handler.
//!
//! Read-only: executable with either `SandboxUse` or `SandboxRead`.

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use sandbox::instance_client::ExecuteRequest;
use serde::Deserialize;

use crate::audit::Audit;
use crate::auth::AuthSubject;
use crate::sandbox::Sandboxes;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{parse_params, schema_for, EntriesOutput};

// ---------------------------------------------------------------------------
// Params
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
struct LsParams {
    /// Absolute path to the directory inside the sandbox workspace.
    path: String,
    /// List of file/directory names to exclude from the listing.
    #[serde(default)]
    ignore: Vec<String>,
}

// ---------------------------------------------------------------------------
// Tool
// ---------------------------------------------------------------------------

pub struct Ls {
    sandboxes: Arc<Sandboxes>,
}

impl Ls {
    pub fn new(sandboxes: Arc<Sandboxes>) -> Self {
        Self { sandboxes }
    }
}

static LS_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<LsParams>);

static LS_OUTPUT_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<EntriesOutput>);

#[async_trait::async_trait]
impl TopLevelTool for Ls {
    fn name(&self) -> &str {
        "LS"
    }

    fn description(&self) -> &str {
        "List directory contents inside the agent's attached sandbox. \
         Read-only — works with both full and read-only sandbox attachments."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &LS_SCHEMA
    }

    fn output_schema(&self) -> Option<&serde_json::Value> {
        Some(&LS_OUTPUT_SCHEMA)
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
        let sandbox_id = subject
            .readable_sandbox_id()
            .ok_or(ToolSetsError::Unauthorized)?;
        Audit::record_action("ls");
        Audit::record_sandbox_id(sandbox_id);
        let params: LsParams = parse_params(arguments)?;

        let client = self
            .sandboxes
            .instance_client_for_read(subject, sandbox_id)
            .await
            .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;

        let req = ExecuteRequest {
            tool: "str_replace_based_edit_tool".to_string(),
            input: serde_json::json!({
                "command": "view",
                "path": params.path,
            }),
        };

        match client.execute(&req).await {
            Ok(resp) => {
                let output = if params.ignore.is_empty() {
                    resp.output
                } else {
                    // Filter out entries matching the ignore list
                    resp.output
                        .lines()
                        .filter(|line| {
                            let name = line.trim_end_matches('/');
                            !params.ignore.iter().any(|ig| ig == name)
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };

                let out = EntriesOutput {
                    entries: output
                        .lines()
                        .filter(|l| !l.is_empty())
                        .map(String::from)
                        .collect(),
                };
                let structured = serde_json::to_value(&out).expect("EntriesOutput serialization");
                let content = vec![Content::text(output)];
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
