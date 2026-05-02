//! `Grep` — content search across files. Two backends:
//!
//! - `space:<slug>/...` paths route through `SpaceFs::grep`.
//! - Anything else forwards to the sandbox-server's `Grep` handler
//!   via `/execute`. Both shell out to `rg`.
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
use super::{OutputSchema, TextOutput};

pub struct Grep {
    sandboxes: Arc<Sandboxes>,
    space_fs: Arc<SpaceFs>,
}

impl Grep {
    pub fn new(sandboxes: Arc<Sandboxes>, space_fs: Arc<SpaceFs>) -> Self {
        Self {
            sandboxes,
            space_fs,
        }
    }
}

static GREP_OUTPUT: LazyLock<OutputSchema<TextOutput>> = LazyLock::new(OutputSchema::new);

static GREP_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "Regular expression pattern to search for in file contents."
            },
            "path": {
                "type": "string",
                "description": "File or directory to search in. Defaults to workspace root, or use `space:<slug>/...` to read from a mounted space."
            },
            "glob": {
                "type": "string",
                "description": "Glob pattern to filter files (e.g. '*.rs', '**/*.{ts,tsx}')."
            },
            "type": {
                "type": "string",
                "description": "File type to search (e.g. 'rust', 'py', 'js')."
            },
            "output_mode": {
                "type": "string",
                "enum": ["files_with_matches", "content", "count"],
                "description": "Output mode. Default: 'files_with_matches'."
            },
            "-i": {
                "type": "boolean",
                "description": "Case insensitive search."
            },
            "-n": {
                "type": "boolean",
                "description": "Show line numbers (only with output_mode='content'). Default: true."
            },
            "-A": {
                "type": "integer",
                "description": "Number of lines to show after each match."
            },
            "-B": {
                "type": "integer",
                "description": "Number of lines to show before each match."
            },
            "-C": {
                "type": "integer",
                "description": "Number of context lines before and after each match."
            },
            "head_limit": {
                "type": "integer",
                "description": "Cap output to first N lines."
            },
            "multiline": {
                "type": "boolean",
                "description": "Enable multiline mode where . matches newlines."
            }
        },
        "required": ["pattern"],
        "additionalProperties": false,
    })
});

#[async_trait::async_trait]
impl TopLevelTool for Grep {
    fn name(&self) -> &str {
        "Grep"
    }

    fn description(&self) -> &str {
        "Search file contents using ripgrep. Accepts either an in-sandbox path \
         or a `space:<slug>/...` path that reads from the project's mounted spaces."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &GREP_SCHEMA
    }

    fn output_schema(&self) -> Option<&serde_json::Value> {
        Some(GREP_OUTPUT.schema())
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
        Audit::record_action("grep");

        let args = arguments.unwrap_or_default();
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let args_value = serde_json::Value::Object(args);

        let space_output = self.space_fs.grep(subject, &path, &args_value).await?;

        if let Some(output) = space_output {
            let out = TextOutput {
                output: output.clone(),
            };
            return Ok(GREP_OUTPUT.success(output, &out));
        }

        let sandbox_id = subject
            .readable_sandbox_id()
            .ok_or(ToolSetsError::Unauthorized)?;
        Audit::record_sandbox_id(sandbox_id);

        let client = self
            .sandboxes
            .instance_client_for_read(subject, sandbox_id)
            .await?;

        // Recover the original args object from the Value we built above.
        let args = match args_value {
            serde_json::Value::Object(o) => o,
            _ => unreachable!("constructed from JsonObject above"),
        };
        let req = ExecuteRequest {
            tool: "Grep".to_string(),
            input: serde_json::Value::Object(args),
        };

        match client.execute(&req).await {
            Ok(resp) => {
                let out = TextOutput {
                    output: resp.output.clone(),
                };
                Ok(if resp.is_error {
                    GREP_OUTPUT.error(resp.output, &out)
                } else {
                    GREP_OUTPUT.success(resp.output, &out)
                })
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "sandbox /execute call failed: {e}"
            ))])),
        }
    }
}
