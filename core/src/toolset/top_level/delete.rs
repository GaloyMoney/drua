//! `Delete` — remove a file. Two backends:
//!
//! - `space:<slug>/...` paths route through `SpaceFs::delete_file`,
//!   which enqueues a `SpaceFileDelete` op and blocks until the
//!   upstream push lands. Idempotent: success even if the file was
//!   already gone.
//! - Anything else forwards to the agent's attached sandbox via
//!   `/execute` with the `Delete` tool name.
//!
//! Mutating: requires `SandboxUse` for sandbox deletes, project-member
//! authz (via `SpaceFs`) for space deletes.

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use sandbox::instance_client::ExecuteRequest;
use serde::Deserialize;

use crate::audit::Audit;
use crate::auth::AuthSubject;
use crate::primitives::{AuthScope, SandboxId};
use crate::sandbox::Sandboxes;
use crate::space_fs::SpaceFs;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{parse_params, schema_for, OutputSchema, TextOutput};

#[derive(Deserialize, schemars::JsonSchema)]
struct DeleteParams {
    path: String,
}

pub struct Delete {
    sandboxes: Arc<Sandboxes>,
    space_fs: Arc<SpaceFs>,
}

impl Delete {
    pub fn new(sandboxes: Arc<Sandboxes>, space_fs: Arc<SpaceFs>) -> Self {
        Self {
            sandboxes,
            space_fs,
        }
    }
}

static DELETE_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<DeleteParams>);
static DELETE_OUTPUT: LazyLock<OutputSchema<TextOutput>> = LazyLock::new(OutputSchema::new);

fn writable_sandbox_id(subject: &AuthSubject) -> Option<SandboxId> {
    subject.scopes().iter().find_map(|s| match s {
        AuthScope::SandboxUse(id) => Some(*id),
        _ => None,
    })
}

#[async_trait::async_trait]
impl TopLevelTool for Delete {
    fn name(&self) -> &str {
        "Delete"
    }

    fn description(&self) -> &str {
        "Delete a file. Accepts an in-sandbox absolute path or a \
         `space:<slug>/...` path from a mounted space. Idempotent — \
         succeeds even if the file is already absent."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &DELETE_SCHEMA
    }

    fn output_schema(&self) -> Option<&serde_json::Value> {
        Some(DELETE_OUTPUT.schema())
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.can_use_agent_file_tools()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: DeleteParams = parse_params(arguments)?;
        Audit::record_action("delete");

        let space_result = self.space_fs.delete_file(subject, &params.path).await?;
        if space_result.is_some() {
            let text = format!("Deleted {}", params.path);
            let out = TextOutput {
                output: text.clone(),
            };
            return Ok(DELETE_OUTPUT.success(text, &out));
        }

        let sandbox_id = writable_sandbox_id(subject).ok_or(ToolSetsError::Unauthorized)?;
        Audit::record_sandbox_id(sandbox_id);

        let client = self
            .sandboxes
            .instance_client_for(subject, sandbox_id)
            .await?;

        let req = ExecuteRequest {
            tool: "Delete".to_string(),
            input: serde_json::json!({ "path": params.path }),
        };

        match client.execute(&req).await {
            Ok(resp) => {
                let out = TextOutput {
                    output: resp.output.clone(),
                };
                Ok(if resp.is_error {
                    DELETE_OUTPUT.error(resp.output, &out)
                } else {
                    DELETE_OUTPUT.success(resp.output, &out)
                })
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "sandbox /execute call failed: {e}"
            ))])),
        }
    }
}
