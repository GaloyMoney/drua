//! `Grep` — content search across files inside the agent's attached sandbox.
//! Wire-compatible with Claude Code's `Grep` tool and ripgrep-style flags.
//!
//! Read-only: executable with either `SandboxUse` or `SandboxRead`.
//! Server-side handler shells out to `rg`.

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use sandbox::instance_client::ExecuteRequest;

use crate::audit::Audit;
use crate::auth::AuthSubject;
use crate::library::{parse_space_path, SpaceFs};
use crate::sandbox::Sandboxes;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{schema_for, TextOutput};

pub struct Grep {
    sandboxes: Arc<Sandboxes>,
    space_fs: Arc<SpaceFs>,
}

impl Grep {
    pub fn new(sandboxes: Arc<Sandboxes>, space_fs: Arc<SpaceFs>) -> Self {
        Self { sandboxes, space_fs }
    }
}

static GREP_OUTPUT_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<TextOutput>);

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
                "description": "File or directory to search in. Defaults to workspace root."
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
        "Search file contents using ripgrep inside the agent's attached sandbox. \
         Supports regex patterns, glob filters, file-type filters, and context \
         lines. Read-only — works with both full and read-only sandbox attachments."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &GREP_SCHEMA
    }

    fn output_schema(&self) -> Option<&serde_json::Value> {
        Some(&GREP_OUTPUT_SCHEMA)
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
        Audit::record_action("grep");

        let args = arguments.unwrap_or_default();
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");

        if let Some(sref) = parse_space_path(path) {
            let args_value = serde_json::Value::Object(args);
            let res = self.space_fs.grep(subject, sref, &args_value).await;
            return match res {
                Ok(output) => {
                    let out = TextOutput {
                        output: output.clone(),
                    };
                    let structured = serde_json::to_value(&out).expect("TextOutput serialization");
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
            tool: "Grep".to_string(),
            input: serde_json::Value::Object(args),
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
