//! `Glob` — file pattern matching. Two backends:
//!
//! - `space:<slug>/...` paths route through `SpaceFs::glob`.
//! - Anything else forwards to the sandbox-server's `Glob` handler
//!   via `/execute`. Both ultimately shell out to `rg --files -g`.
//!
//! Read-only: executable with either `SandboxUse` or `SandboxRead`.

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use sandbox::GlobInput;

use crate::audit::Audit;
use crate::auth::AuthSubject;
use crate::sandbox::Sandboxes;
use crate::space_fs::SpaceFs;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{parse_params, schema_for, FilesOutput, OutputSchema};

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

static GLOB_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<GlobInput>);

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

    fn inner_output_schema(&self) -> Option<&serde_json::Value> {
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
        let input: GlobInput = parse_params(arguments)?;
        Audit::record_action("glob");

        let path_for_space = input.path.as_deref().unwrap_or("");
        let space_files = self
            .space_fs
            .glob(subject, path_for_space, &input.pattern)
            .await?;

        if let Some(files) = space_files {
            let text = files.join("\n");
            let out = FilesOutput { files };
            return Ok(GLOB_OUTPUT.success(text, &out));
        }

        let sandbox_id = subject
            .readable_sandbox_id()
            .ok_or_else(|| super::sandbox_read_denied("Glob"))?;
        Audit::record_sandbox_id(sandbox_id);

        let client = self
            .sandboxes
            .instance_client_for_read(subject, sandbox_id)
            .await?;

        match client.execute_glob(&input).await {
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
