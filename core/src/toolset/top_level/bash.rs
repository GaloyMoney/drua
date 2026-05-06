//! `bash` — run a shell command inside the agent's currently attached
//! sandbox. Compatible with Anthropic's built-in `bash` tool
//! (`bash_20250124`): same `name`, same `{ command, restart }`
//! semantics, plus drua's optional `timeout_ms` extension. Non-zero exits
//! surface with the same `is_error: true` semantics, so prompts that
//! target the built-in keep working without changes.
//!
//! Visibility / authz:
//! - Visible only to [`AuthSubject::Agent`] / [`AuthSubject::AgentOnBehalfOfUser`]
//!   — users / exported-agent tokens / anonymous never see it.
//! - Executable only when the subject carries a [`AuthScope::SandboxUse`]
//!   (granted by attaching as Write). Read-only attachment isn't enough —
//!   bash can mutate sandbox state. Visible-but-unauthorized when an agent
//!   has no write attachment yet, so the model can ask to attach and try
//!   again instead of being baffled by a missing tool.

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use sandbox::BashCommandInput;

use crate::audit::Audit;
use crate::auth::AuthSubject;
use crate::primitives::{AuthScope, SandboxId};
use crate::sandbox::Sandboxes;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{schema_for, TextOutput};

pub struct Bash {
    sandboxes: Arc<Sandboxes>,
}

impl Bash {
    pub fn new(sandboxes: Arc<Sandboxes>) -> Self {
        Self { sandboxes }
    }
}

static BASH_OUTPUT_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<TextOutput>);

static BASH_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    // Mirrors Anthropic's bash_20250124 command/restart fields and adds
    // timeout_ms as a drua extension. The schema's bounds reference
    // [`BashCommandInput`] constants so the typed contract and the model-
    // facing schema can't drift.
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
            },
            "timeout_ms": {
                "type": "integer",
                "minimum": 1,
                "maximum": BashCommandInput::MAX_TIMEOUT_MS,
                "description": "Maximum wall-clock time for this command in milliseconds. Defaults to 120000. Use this for long-running builds or tests instead of wrapping the command in timeout(1)."
            }
        },
        "additionalProperties": false,
    })
});

/// First `SandboxUse` scope (writer attachment). Read-only attachments don't
/// qualify because `bash` can mutate state. Entity enforces a single active
/// attachment per agent, but first-wins regardless.
fn sandbox_use_id(subject: &AuthSubject) -> Option<SandboxId> {
    subject.scopes().iter().find_map(|s| match s {
        AuthScope::SandboxUse(id) => Some(*id),
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
         Compatible with Anthropic's built-in bash tool — same \
         command / restart semantics, plus optional timeout_ms for \
         long-running commands. Same is_error semantics. \
         Output is stdout + stderr concatenated; exit code != 0 surfaces \
         as is_error: true on the tool result."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &BASH_SCHEMA
    }

    fn output_schema(&self) -> Option<&serde_json::Value> {
        Some(&BASH_OUTPUT_SCHEMA)
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        // Visible whether attached or not so the model can ask to attach.
        subject.can_use_agent_file_tools()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let sandbox_id = sandbox_use_id(subject).ok_or(ToolSetsError::Unauthorized)?;
        Audit::record_action("bash");
        Audit::record_sandbox_id(sandbox_id);

        let client = self
            .sandboxes
            .instance_client_for(subject, sandbox_id)
            .await?;

        // MCP boundary: `arguments` is loosely-typed JSON. Convert to the
        // typed `BashCommandInput` here so any caller-side typo (e.g.
        // `timeoutMs`) surfaces as a clear deserialization error rather
        // than being silently dropped on the floor.
        let raw = serde_json::Value::Object(arguments.unwrap_or_default());
        let input: BashCommandInput = match serde_json::from_value(raw) {
            Ok(input) => input,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Invalid bash input: {e}"
                ))]));
            }
        };

        // Map transport/server failures to `is_error: true` text so the model
        // sees a consistent shape. Non-zero exits arrive with `is_error` set already.
        match client.execute_bash(&input).await {
            Ok(resp) => {
                let is_error = resp.is_error;
                let out = TextOutput {
                    output: resp.output,
                };
                let structured = serde_json::to_value(&out).expect("TextOutput serialization");
                // Move `out.output` into Content::text rather than borrowing,
                // so we don't clone potentially-large bash output.
                let content = vec![Content::text(out.output)];
                let mut result = if is_error {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_exposes_timeout_ms_extension() {
        let timeout_schema = BASH_SCHEMA["properties"]["timeout_ms"]
            .as_object()
            .expect("timeout_ms schema should be present");

        assert_eq!(timeout_schema["type"], "integer");
        assert_eq!(timeout_schema["maximum"], BashCommandInput::MAX_TIMEOUT_MS);
    }
}
