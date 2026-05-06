//! `Grep` — content search across files. Two backends:
//!
//! - `space:<slug>/...` paths route through `SpaceFs::grep`.
//! - Anything else forwards to the sandbox-server's `Grep` handler
//!   via `/execute`. Both shell out to `rg`.
//!
//! Read-only: executable with either `SandboxUse` or `SandboxRead`.

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use sandbox::GrepInput;

use crate::audit::Audit;
use crate::auth::AuthSubject;
use crate::sandbox::Sandboxes;
use crate::space_fs::SpaceFs;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{parse_params, schema_for, OutputSchema, TextOutput};

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
static GREP_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<GrepInput>);

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
        let input: GrepInput = parse_params(arguments)?;
        Audit::record_action("grep");

        let path_for_space = input.path.as_deref().unwrap_or("");
        let space_output = self.space_fs.grep(subject, path_for_space, &input).await?;

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

        match client.execute_grep(&input).await {
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
