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
use serde::Deserialize;

use crate::audit::Audit;
use crate::auth::AuthSubject;
use crate::sandbox::Sandboxes;
use crate::space_fs::SpaceFs;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{parse_params, render_detailed, schema_for, FilesOutput, OutputSchema};

/// Local mirror of `sandbox::GlobInput` plus `details`. Kept separate
/// rather than adding a field to `GlobInput`: that type is
/// `deny_unknown_fields` and sent on the wire to running sandbox
/// processes, including ones on an older binary that would reject an
/// unrecognised field.
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct GlobParams {
    /// Glob pattern to match files (e.g. `**/*.rs`, `src/**/*.ts`).
    pattern: String,

    /// Directory to search in. Defaults to workspace root, or use
    /// `space:<slug>/...` to read from a mounted space.
    #[serde(default)]
    path: Option<String>,

    /// `space:` paths only — append each file's first-commit and
    /// last-commit dates (`created=`, `modified=`, UTC days).
    #[serde(default)]
    details: bool,
}

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

static GLOB_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<GlobParams>);

#[async_trait::async_trait]
impl TopLevelTool for GlobTool {
    fn name(&self) -> &str {
        "Glob"
    }

    fn description(&self) -> &str {
        "Find files matching a glob pattern. Accepts either an in-sandbox path \
         or a `space:<slug>/...` path that reads from the project's mounted spaces. \
         Pass `details: true` on a `space:` path to get each file's first- and \
         last-commit dates."
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
        let params: GlobParams = parse_params(arguments)?;
        Audit::record_action("glob");

        let path_for_space = params.path.as_deref().unwrap_or("");

        if params.details {
            let space_entries = self
                .space_fs
                .glob_detailed(subject, path_for_space, &params.pattern)
                .await?;
            if let Some(entries) = space_entries {
                let (files, text, details) = render_detailed(entries);
                let out = FilesOutput { files, details };
                return Ok(GLOB_OUTPUT.success(text, &out));
            }
            return Err(ToolSetsError::InvalidArgument(
                "details is only supported for space: paths".into(),
            ));
        }

        let space_files = self
            .space_fs
            .glob(subject, path_for_space, &params.pattern)
            .await?;

        if let Some(files) = space_files {
            let text = files.join("\n");
            let out = FilesOutput {
                files,
                details: None,
            };
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

        let input = GlobInput {
            pattern: params.pattern,
            path: params.path,
        };

        match client.execute_glob(&input).await {
            Ok(resp) => {
                let files: Vec<String> = resp
                    .output
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(String::from)
                    .collect();
                let text = files.join("\n");
                let out = FilesOutput {
                    files,
                    details: None,
                };
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
