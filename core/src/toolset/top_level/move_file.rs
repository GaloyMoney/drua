//! `MoveFile` — rename a file. Two backends:
//!
//! - When **both** `from` and `to` are `space:<slug>/...` paths in the
//!   same space, routes through `SpaceFs::move_file`, which enqueues
//!   a `SpaceFileMove` op and blocks until the upstream push lands.
//!   Cross-space (different slugs) and mixed (one space, one sandbox)
//!   moves are rejected — there's no v1 use case and silent fall-through
//!   would be surprising.
//! - When both paths are in-sandbox, forwards `{ from, to }` to the
//!   sandbox via `/execute` with the `Move` tool name.
//!
//! Mutating: requires `SandboxUse` for sandbox moves, project-member
//! authz (via `SpaceFs`) for space moves.

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use sandbox::MoveInput;

use crate::audit::Audit;
use crate::auth::AuthSubject;
use crate::primitives::{AuthScope, SandboxId};
use crate::sandbox::Sandboxes;
use crate::space_fs::SpaceFs;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{parse_params, schema_for, OutputSchema, TextOutput};

pub struct MoveFile {
    sandboxes: Arc<Sandboxes>,
    space_fs: Arc<SpaceFs>,
}

impl MoveFile {
    pub fn new(sandboxes: Arc<Sandboxes>, space_fs: Arc<SpaceFs>) -> Self {
        Self {
            sandboxes,
            space_fs,
        }
    }
}

static MOVE_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<MoveInput>);
static MOVE_OUTPUT: LazyLock<OutputSchema<TextOutput>> = LazyLock::new(OutputSchema::new);

fn writable_sandbox_id(subject: &AuthSubject) -> Option<SandboxId> {
    subject.scopes().iter().find_map(|s| match s {
        AuthScope::SandboxUse(id) => Some(*id),
        _ => None,
    })
}

#[async_trait::async_trait]
impl TopLevelTool for MoveFile {
    fn name(&self) -> &str {
        "Move"
    }

    fn description(&self) -> &str {
        "Rename a file. Accepts in-sandbox absolute paths or \
         `space:<slug>/...` paths from mounted spaces. Both `from` and \
         `to` must be in the same realm (same space, or both in-sandbox); \
         cross-space moves are not supported. Errors if the destination \
         already exists."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &MOVE_SCHEMA
    }

    fn inner_output_schema(&self) -> Option<&serde_json::Value> {
        Some(MOVE_OUTPUT.schema())
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.can_use_agent_file_tools()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let input: MoveInput = parse_params(arguments)?;
        Audit::record_action("move");

        let space_result = self
            .space_fs
            .move_file(subject, &input.from, &input.to)
            .await?;
        if space_result.is_some() {
            let text = format!("Moved {} -> {}", input.from, input.to);
            let out = TextOutput {
                output: text.clone(),
            };
            return Ok(MOVE_OUTPUT.success(text, &out));
        }

        let sandbox_id = writable_sandbox_id(subject).ok_or(ToolSetsError::Unauthorized)?;
        Audit::record_sandbox_id(sandbox_id);

        let client = self
            .sandboxes
            .instance_client_for(subject, sandbox_id)
            .await?;

        match client.execute_move(&input).await {
            Ok(resp) => {
                let out = TextOutput {
                    output: resp.output.clone(),
                };
                Ok(if resp.is_error {
                    MOVE_OUTPUT.error(resp.output, &out)
                } else {
                    MOVE_OUTPUT.success(resp.output, &out)
                })
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "sandbox /execute call failed: {e}"
            ))])),
        }
    }
}
