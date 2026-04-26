//! `workspace_agent` — consolidated workspace-scoped agent management.
//!
//! Single tool with a `command` discriminator (like `text_editor`):
//! `create`, `list`, `attach_sandbox`, `detach_sandbox`.
//!
//! Read commands (`list`) require `can_read_workspace`; write commands
//! (`create`, `attach_sandbox`, `detach_sandbox`) enforce
//! `can_write_workspace` inside `call()`.

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use serde::Deserialize;

use crate::agent::{Agent, AgentRole, Agents};
use crate::audit::Audit;
use crate::auth::AuthSubject;
use crate::primitives::{AgentId, SandboxId};
use crate::sandbox::{SandboxAgentMode, Sandboxes};

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::parse_params;

// ---------------------------------------------------------------------------
// Params
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
struct WorkspaceAgentParams {
    /// Which agent operation to perform.
    command: WorkspaceAgentCommand,
    /// Display name for the new agent (required for `create`).
    name: Option<String>,
    /// ID of the agent (required for `attach_sandbox` and `detach_sandbox`).
    #[schemars(with = "Option<uuid::Uuid>")]
    agent_id: Option<AgentId>,
    /// ID of the sandbox (required for `attach_sandbox` and `detach_sandbox`).
    #[schemars(with = "Option<uuid::Uuid>")]
    sandbox_id: Option<SandboxId>,
    /// Attach mode — 'read' or 'use'. Defaults to 'read'. Only used for `attach_sandbox`.
    mode: Option<SandboxAgentMode>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum WorkspaceAgentCommand {
    /// Create a new agent in the workspace.
    Create,
    /// List all agents in the workspace.
    List,
    /// Attach a sandbox to an agent.
    AttachSandbox,
    /// Detach a sandbox from an agent.
    DetachSandbox,
}

impl WorkspaceAgentCommand {
    fn audit_action(&self) -> &'static str {
        match self {
            Self::Create => "agent.create",
            Self::List => "agent.list",
            Self::AttachSandbox => "agent.attach_sandbox",
            Self::DetachSandbox => "agent.detach_sandbox",
        }
    }
}

// ---------------------------------------------------------------------------
// Tool
// ---------------------------------------------------------------------------

pub struct WorkspaceAgent {
    agents: Arc<Agents>,
    sandboxes: Arc<Sandboxes>,
}

impl WorkspaceAgent {
    pub fn new(agents: Arc<Agents>, sandboxes: Arc<Sandboxes>) -> Self {
        Self { agents, sandboxes }
    }
}

static SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    let settings = schemars::gen::SchemaSettings::draft07().with(|s| {
        s.inline_subschemas = true;
        s.meta_schema = None;
    });
    let generator = settings.into_generator();
    let schema = generator.into_root_schema_for::<WorkspaceAgentParams>();
    let mut value = serde_json::to_value(schema).expect("schema serialization");
    if let Some(obj) = value.as_object_mut() {
        obj.remove("title");
        obj.remove("definitions");
        obj.insert(
            "additionalProperties".into(),
            serde_json::Value::Bool(false),
        );
    }
    value
});

#[async_trait::async_trait]
impl TopLevelTool for WorkspaceAgent {
    fn name(&self) -> &str {
        "agent"
    }

    fn description(&self) -> &str {
        "Manage agents. Commands: `create` (requires `name`), `list`, \
         `attach_sandbox` (requires `agent_id`, `sandbox_id`, optional `mode`), \
         `detach_sandbox` (requires `agent_id`, `sandbox_id`)."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.can_read_workspace()
    }

    fn composable(&self) -> bool {
        false
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let workspace_id = subject.workspace_id().ok_or(ToolSetsError::Unauthorized)?;
        let params: WorkspaceAgentParams = parse_params(arguments)?;

        Audit::record_action(params.command.audit_action());

        match params.command {
            WorkspaceAgentCommand::Create => {
                if !subject.can_write_workspace() {
                    return Err(ToolSetsError::Unauthorized);
                }
                let name = params.name.ok_or_else(|| {
                    ToolSetsError::MissingArgument("name is required for create".to_string())
                })?;
                let agent = self
                    .agents
                    .create_agent(subject, workspace_id, &name, None)
                    .await
                    .map_err(|e| ToolSetsError::Agent(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(format_agent(
                    &agent,
                ))]))
            }

            WorkspaceAgentCommand::List => {
                let agents = self
                    .agents
                    .list_for_workspace(subject, workspace_id)
                    .await
                    .map_err(|e| ToolSetsError::Agent(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(format_agents(
                    &agents,
                ))]))
            }

            WorkspaceAgentCommand::AttachSandbox => {
                if !subject.can_write_workspace() {
                    return Err(ToolSetsError::Unauthorized);
                }
                let agent_id = params.agent_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument(
                        "agent_id is required for attach_sandbox".to_string(),
                    )
                })?;
                let sandbox_id = params.sandbox_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument(
                        "sandbox_id is required for attach_sandbox".to_string(),
                    )
                })?;
                let mode = params.mode.unwrap_or(SandboxAgentMode::Read);

                let sandbox = self
                    .sandboxes
                    .find_by_id(subject, sandbox_id)
                    .await
                    .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;
                if sandbox.workspace_id != workspace_id {
                    return Err(ToolSetsError::Unauthorized);
                }

                let agent = self
                    .agents
                    .attach_sandbox(subject, agent_id, sandbox_id, mode)
                    .await
                    .map_err(|e| ToolSetsError::Agent(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(format_agent(
                    &agent,
                ))]))
            }

            WorkspaceAgentCommand::DetachSandbox => {
                if !subject.can_write_workspace() {
                    return Err(ToolSetsError::Unauthorized);
                }
                let agent_id = params.agent_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument(
                        "agent_id is required for detach_sandbox".to_string(),
                    )
                })?;
                let sandbox_id = params.sandbox_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument(
                        "sandbox_id is required for detach_sandbox".to_string(),
                    )
                })?;

                let existing = self
                    .agents
                    .find_by_id(subject, agent_id)
                    .await
                    .map_err(|e| ToolSetsError::Agent(e.to_string()))?;
                if existing.workspace_id != workspace_id {
                    return Err(ToolSetsError::Unauthorized);
                }

                let sandbox = self
                    .sandboxes
                    .find_by_id(subject, sandbox_id)
                    .await
                    .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;
                if sandbox.workspace_id != workspace_id {
                    return Err(ToolSetsError::Unauthorized);
                }

                let agent = self
                    .agents
                    .detach_sandbox(subject, agent_id, sandbox_id)
                    .await
                    .map_err(|e| ToolSetsError::Agent(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(format_agent(
                    &agent,
                ))]))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn format_agent(a: &Agent) -> String {
    let role = match a.agent_role {
        AgentRole::WorkspaceLead => "workspace_lead",
        AgentRole::Agent => "agent",
    };
    let sandbox = match &a.attached_sandbox {
        Some((sid, mode)) => format!("{sid} ({mode:?})"),
        None => "none".to_string(),
    };
    format!(
        "Agent created.\n  id: {}\n  name: {}\n  role: {}\n  workspace: {}\n  sandbox: {}",
        a.id, a.name, role, a.workspace_id, sandbox
    )
}

fn format_agents(agents: &[Agent]) -> String {
    if agents.is_empty() {
        return "No agents found.".to_string();
    }

    let mut lines = Vec::with_capacity(agents.len() + 2);
    lines.push(format!(
        "{:<38} {:<20} {:<16} {:<38}",
        "ID", "NAME", "ROLE", "SANDBOX"
    ));
    lines.push("-".repeat(116));

    for a in agents {
        let sandbox = match &a.attached_sandbox {
            Some((sid, mode)) => format!("{sid} ({mode:?})"),
            None => "—".to_string(),
        };
        let role = match a.agent_role {
            AgentRole::WorkspaceLead => "workspace_lead",
            AgentRole::Agent => "agent",
        };
        lines.push(format!(
            "{:<38} {:<20} {:<16} {:<38}",
            a.id,
            truncate(&a.name, 20),
            role,
            sandbox,
        ));
    }

    lines.join("\n")
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
    }
}
