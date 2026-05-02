//! `Glob` — file pattern matching. Two backends:
//!
//! - `space:<slug>/...` paths route through `SpaceFs::glob`.
//! - Anything else forwards to the sandbox-server's `Glob` handler
//!   via `/execute`. Both ultimately shell out to `rg --files -g`.
//!
//! Read-only: executable with either `SandboxUse` or `SandboxRead`.

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use sandbox::instance_client::ExecuteRequest;

use crate::audit::Audit;
use crate::auth::AuthSubject;
use crate::sandbox::Sandboxes;
use crate::space_fs::SpaceFs;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{FilesOutput, OutputSchema};

pub struct GlobTool {
    sandboxes: Arc<Sandboxes>,
    space_fs: Arc<SpaceFs>,
}

impl GlobTool {
    pub fn new(sandboxes: Arc<Sandboxes>, space_fs: Arc<SpaceFs>) -> Self {
        Self {
            sandboxes,
            space_fs,
        }
    }
}

static GLOB_OUTPUT: LazyLock<OutputSchema<FilesOutput>> = LazyLock::new(OutputSchema::new);

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
                "description": "Directory to search in. Defaults to workspace root, or use `space:<slug>/...` to read from a mounted space."
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
        "Find files matching a glob pattern. Accepts either an in-sandbox path \
         or a `space:<slug>/...` path that reads from the project's mounted spaces."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &GLOB_SCHEMA
    }

    fn output_schema(&self) -> Option<&serde_json::Value> {
        Some(GLOB_OUTPUT.schema())
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        // See bash.rs.
        subject.can_use_agent_file_tools()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        Audit::record_action("glob");

        let args = arguments.unwrap_or_default();
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolSetsError::MissingArgument("pattern".to_string()))?
            .to_string();

        let space_files = self.space_fs.glob(subject, &path, &pattern).await?;

        if let Some(files) = space_files {
            let text = files.join("\n");
            let out = FilesOutput { files };
            return Ok(GLOB_OUTPUT.success(text, &out));
        }

        let sandbox_id = subject
            .readable_sandbox_id()
            .ok_or(ToolSetsError::Unauthorized)?;
        Audit::record_sandbox_id(sandbox_id);

        let client = self
            .sandboxes
            .instance_client_for_read(subject, sandbox_id)
            .await?;

        let req = ExecuteRequest {
            tool: "Glob".to_string(),
            input: serde_json::Value::Object(args),
        };

        match client.execute(&req).await {
            Ok(resp) => {
                let files: Vec<String> = resp
                    .output
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(String::from)
                    .collect();
                let text = files.join("\n");
                let out = FilesOutput { files };
                Ok(if resp.is_error {
                    GLOB_OUTPUT.error(text, &out)
                } else {
                    GLOB_OUTPUT.success(text, &out)
                })
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "sandbox /execute call failed: {e}"
            ))])),
        }
    }
}
