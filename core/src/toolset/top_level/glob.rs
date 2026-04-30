//! `Glob` — file pattern matching inside the agent's attached sandbox.
//! Returns matching file paths sorted by modification time (most recent first).
//!
//! Read-only: executable with either `SandboxUse` or `SandboxRead`.
//! Server-side handler uses `rg --files -g <pattern>`.

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use sandbox::instance_client::ExecuteRequest;

use crate::audit::Audit;
use crate::auth::AuthSubject;
use crate::library::{parse_space_path, SpaceFs};
use crate::sandbox::Sandboxes;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{schema_for, FilesOutput};

pub struct GlobTool {
    sandboxes: Arc<Sandboxes>,
    space_fs: Arc<SpaceFs>,
}

impl GlobTool {
    pub fn new(sandboxes: Arc<Sandboxes>, space_fs: Arc<SpaceFs>) -> Self {
        Self { sandboxes, space_fs }
    }
}

static GLOB_OUTPUT_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<FilesOutput>);

static GLOB_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "Glob pattern to match files (e.g. '**/*.rs', 'src/**/*.ts')."
            },
            "path": {
                "type": "string",
                "description": "Directory to search in. Defaults to workspace root."
            }
        },
        "required": ["pattern"],
        "additionalProperties": false,
    })
});

#[async_trait::async_trait]
impl TopLevelTool for GlobTool {
    fn name(&self) -> &str {
        "Glob"
    }

    fn description(&self) -> &str {
        "Find files matching a glob pattern inside the agent's attached sandbox. \
         Returns matching file paths. Read-only — works with both full and \
         read-only sandbox attachments."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &GLOB_SCHEMA
    }

    fn output_schema(&self) -> Option<&serde_json::Value> {
        Some(&GLOB_OUTPUT_SCHEMA)
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
        Audit::record_action("glob");

        let args = arguments.unwrap_or_default();
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolSetsError::MissingArgument("pattern".to_string()))?;

        if let Some(sref) = parse_space_path(path) {
            let res = self.space_fs.glob(subject, sref, pattern).await;
            return match res {
                Ok(output) => {
                    let out = FilesOutput {
                        files: output
                            .lines()
                            .filter(|l| !l.is_empty())
                            .map(String::from)
                            .collect(),
                    };
                    let structured = serde_json::to_value(&out).expect("FilesOutput serialization");
                    let mut result = CallToolResult::success(vec![Content::text(output)]);
                    result.structured_content = Some(structured);
                    Ok(result)
                }
                Err(e) => Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
            };
        }

        let sandbox_id = subject
            .readable_sandbox_id()
            .ok_or(ToolSetsError::Unauthorized)?;
        Audit::record_sandbox_id(sandbox_id);

        let client = self
            .sandboxes
            .instance_client_for_read(subject, sandbox_id)
            .await
            .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;

        let req = ExecuteRequest {
            tool: "Glob".to_string(),
            input: serde_json::Value::Object(args),
        };

        match client.execute(&req).await {
            Ok(resp) => {
                let out = FilesOutput {
                    files: resp
                        .output
                        .lines()
                        .filter(|l| !l.is_empty())
                        .map(String::from)
                        .collect(),
                };
                let structured = serde_json::to_value(&out).expect("FilesOutput serialization");
                let content = vec![Content::text(resp.output)];
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
