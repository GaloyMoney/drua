//! `bash` — run a shell command inside the agent's currently attached
//! sandbox. Wire-compatible with Anthropic's built-in `bash` tool
//! (`bash_20250124`): same `name`, same `{ command, restart }` input
//! schema, same `is_error: true` semantics on non-zero exit, so prompts
//! that target the built-in keep working without changes.
//!
//! Visibility / authz:
//! - Visible only to [`AuthSubject::Agent`] / [`AuthSubject::AgentOnBehalfOfUser`]
//!   — users / exported-agent tokens / anonymous never see it.
//! - Executable only when the subject carries a [`AuthScope::SandboxUseAll`]
//!   (granted by attaching as Write). Read-only attachment isn't enough —
//!   bash can mutate sandbox state. Visible-but-unauthorized when an agent
//!   has no write attachment yet, so the model can ask to attach and try
//!   again instead of being baffled by a missing tool.

use std::sync::LazyLock;

use rmcp::model::{CallToolResult, Content, JsonObject};
use sandbox::instance_client::ExecuteRequest;

use crate::auth::AuthSubject;
use crate::primitives::{AuthScope, SandboxId};
use crate::sandbox::Sandboxes;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::is_agent_subject;

pub struct Bash {
    sandboxes: Sandboxes,
}

impl Bash {
    pub fn new(sandboxes: Sandboxes) -> Self {
        Self { sandboxes }
    }
}

static BASH_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    // Mirrors Anthropic's built-in bash tool (bash_20250124): a single
    // optional `command` string and an optional `restart` flag. The
    // server forwards the entire object as the tool input, so anything
    // beyond these two fields is ignored.
    serde_json::json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "description": "The bash command to run. Required unless restart is true."
            },
            "restart": {
                "type": "boolean",
                "description": "If true, reset the persistent bash session (no-op when the server is stateless)."
            }
        },
        "additionalProperties": false,
    })
});

/// First [`SandboxId`] from a `SandboxUseAll` scope on the subject — i.e.
/// the sandbox the agent is currently attached to as a writer. We expect
/// at most one such scope per agent today (the entity enforces a single
/// active attachment); first one wins regardless. Read-only attachments
/// don't qualify because `bash` can mutate state.
fn sandbox_use_id(subject: &AuthSubject) -> Option<SandboxId> {
    subject.scopes().iter().find_map(|s| match s {
        AuthScope::SandboxUseAll(id) => Some(*id),
        _ => None,
    })
}

#[async_trait::async_trait]
impl TopLevelTool for Bash {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Run a shell command inside the agent's attached sandbox. \
         Wire-compatible with Anthropic's built-in bash tool — same \
         input schema (command / restart) and same is_error semantics. \
         Output is stdout + stderr concatenated; exit code != 0 surfaces \
         as is_error: true on the tool result."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &BASH_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        is_agent_subject(subject)
    }

    fn can_execute(&self, subject: &AuthSubject) -> bool {
        // Visible-but-unauthorized when an agent has no attachment yet —
        // the model gets a clear `Unauthorized` error from dispatch
        // instead of the tool silently disappearing.
        is_agent_subject(subject) && sandbox_use_id(subject).is_some()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        // Defense in depth — dispatch already checked `can_execute`, but
        // surfacing the same error here keeps the tool callable in
        // contexts that bypass the dispatcher (tests, internal calls).
        let sandbox_id = sandbox_use_id(subject).ok_or(ToolSetsError::Unauthorized)?;

        let client = self
            .sandboxes
            .instance_client_for(sandbox_id)
            .await
            .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;

        let req = ExecuteRequest {
            tool: "bash".to_string(),
            input: serde_json::Value::Object(arguments.unwrap_or_default()),
        };

        // Map every transport / server failure to `is_error: true` text
        // so the model sees one consistent shape instead of an opaque
        // `ToolSetsError`. Genuine bash failures (non-zero exit) come
        // back from the server with `is_error: true` already; we just
        // forward the flag.
        match client.execute(&req).await {
            Ok(resp) => {
                let content = vec![Content::text(resp.output)];
                if resp.is_error {
                    Ok(CallToolResult::error(content))
                } else {
                    Ok(CallToolResult::success(content))
                }
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "sandbox /execute call failed: {e}"
            ))])),
        }
    }
}
