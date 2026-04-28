//! `bash` — run a shell command inside the agent's currently attached
//! sandbox. Wire-compatible with Anthropic's built-in `bash` tool
//! (`bash_20250124`): same `name`, same `{ command, restart }` input
//! schema, same `is_error: true` semantics on non-zero exit, so prompts
//! that target the built-in keep working without changes.
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
use sandbox::instance_client::ExecuteRequest;

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
    // Mirrors Anthropic's bash_20250124. Server forwards the object
    // verbatim; extra fields are ignored.
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
         Wire-compatible with Anthropic's built-in bash tool — same \
         input schema (command / restart) and same is_error semantics. \
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
        // Hidden from workspace admins (they don't attach sandboxes).
        // Other agents see it whether attached or not so the model can ask to attach.
        subject.is_agent() && !subject.is_workspace_admin()
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
            .await
            .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;

        let req = ExecuteRequest {
            tool: "bash".to_string(),
            input: serde_json::Value::Object(arguments.unwrap_or_default()),
        };

        // Map transport/server failures to `is_error: true` text so the model
        // sees a consistent shape. Non-zero exits arrive with `is_error` set already.
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
