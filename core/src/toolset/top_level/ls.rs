//! `LS` — directory listing. Two backends:
//!
//! - `space:<slug>/...` paths route through `SpaceFs` (no sandbox needed).
//! - Anything else translates `{path, ignore}` into the text editor's
//!   `view` command and forwards to the agent's attached sandbox via
//!   the `/execute` handler.
//!
//! Read-only: executable with either `SandboxUse` or `SandboxRead`.

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use sandbox::instance_client::ExecuteRequest;
use serde::Deserialize;

use crate::audit::Audit;
use crate::auth::AuthSubject;
use crate::sandbox::Sandboxes;
use crate::space_fs::SpaceFs;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{parse_params, schema_for, EntriesOutput, OutputSchema};

#[derive(Deserialize, schemars::JsonSchema)]
struct LsParams {
    path: String,
    #[serde(default)]
    ignore: Vec<String>,
}

pub struct Ls {
    sandboxes: Arc<Sandboxes>,
    space_fs: Arc<SpaceFs>,
}

impl Ls {
    pub fn new(sandboxes: Arc<Sandboxes>, space_fs: Arc<SpaceFs>) -> Self {
        Self {
            sandboxes,
            space_fs,
        }
    }
}

static LS_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<LsParams>);
static LS_OUTPUT: LazyLock<OutputSchema<EntriesOutput>> = LazyLock::new(OutputSchema::new);

#[async_trait::async_trait]
impl TopLevelTool for Ls {
    fn name(&self) -> &str {
        "LS"
    }

    fn description(&self) -> &str {
        "List directory contents. Accepts either an in-sandbox path or a \
         `space:<slug>/...` path that reads from the project's mounted spaces."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &LS_SCHEMA
    }

    fn output_schema(&self) -> Option<&serde_json::Value> {
        Some(LS_OUTPUT.schema())
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.can_use_agent_file_tools()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: LsParams = parse_params(arguments)?;
        Audit::record_action("ls");

        let space_entries = self.space_fs.view_dir(subject, &params.path).await?;

        if let Some(entries) = space_entries {
            let filtered = filter_ignored(entries, &params.ignore);
            let text = filtered.join("\n");
            let out = EntriesOutput { entries: filtered };
            return Ok(LS_OUTPUT.success(text, &out));
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
            tool: "str_replace_based_edit_tool".to_string(),
            input: serde_json::json!({
                "command": "view",
                "path": params.path,
            }),
        };

        match client.execute(&req).await {
            Ok(resp) => {
                let entries: Vec<String> = resp.output.lines().map(String::from).collect();
                let filtered = filter_ignored(entries, &params.ignore);
                let text = filtered.join("\n");
                let out = EntriesOutput { entries: filtered };
                Ok(if resp.is_error {
                    LS_OUTPUT.error(text, &out)
                } else {
                    LS_OUTPUT.success(text, &out)
                })
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "sandbox /execute call failed: {e}"
            ))])),
        }
    }
}

/// Drops empty lines, then any entry whose basename matches an
/// `ignore` pattern (trailing slash-stripped to handle directories).
fn filter_ignored(entries: Vec<String>, ignore: &[String]) -> Vec<String> {
    entries
        .into_iter()
        .filter(|l| !l.is_empty())
        .filter(|name| {
            let trimmed = name.trim_end_matches('/');
            !ignore.iter().any(|ig| ig == trimmed)
        })
        .collect()
}
