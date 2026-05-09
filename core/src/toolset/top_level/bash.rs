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
use sandbox::{BashCommandInput, BashCommandOutput};

use crate::audit::Audit;
use crate::auth::AuthSubject;
use crate::primitives::{AuthScope, SandboxId};
use crate::sandbox::Sandboxes;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::schema_for;

pub struct Bash {
    sandboxes: Arc<Sandboxes>,
}

impl Bash {
    pub fn new(sandboxes: Arc<Sandboxes>) -> Self {
        Self { sandboxes }
    }
}

static BASH_OUTPUT_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<BashCommandOutput>);

static BASH_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
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
         long-running commands. Returns structured output \
         {stdout, stderr, exit_code, duration_ms}; exit_code != 0 \
         surfaces as is_error: true on the tool result."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &BASH_SCHEMA
    }

    fn output_schema(&self) -> Option<&serde_json::Value> {
        Some(&BASH_OUTPUT_SCHEMA)
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
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

        let raw = serde_json::Value::Object(arguments.unwrap_or_default());
        let input: BashCommandInput = match serde_json::from_value(raw) {
            Ok(input) => input,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Invalid bash input: {e}"
                ))]));
            }
        };

        match client.execute_bash(&input).await {
            Ok((output, transport_error)) => {
                let exit_code = output.exit_code;
                let is_error = transport_error || exit_code != 0;
                let structured = serde_json::to_value(&output).expect("BashCommandOutput");
                let default_text = format!(
                    "[bash exit={} in {}ms; stdout={}B stderr={}B]\n{}",
                    output.exit_code,
                    output.duration_ms,
                    output.stdout.len(),
                    output.stderr.len(),
                    if output.stderr.is_empty() {
                        output.stdout.clone()
                    } else if output.stdout.is_empty() {
                        format!("--- stderr ---\n{}", output.stderr)
                    } else {
                        format!(
                            "--- stdout ---\n{}\n--- stderr ---\n{}",
                            output.stdout, output.stderr
                        )
                    },
                );
                let content = vec![Content::text(default_text)];
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
