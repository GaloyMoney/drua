//! `project_agent` — consolidated project-scoped agent management.
//!
//! Single tool with a `command` discriminator (like `text_editor`):
//! `create`, `list`, `attach_sandbox`, `detach_sandbox`.
//!
//! Authorization is delegated entirely to [`Agents`] / [`Sandboxes`]:
//! every service method runs `subject.can(verb, resource)` itself, so
//! this layer only routes parameters and surfaces errors.

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use serde::Deserialize;

use crate::agent::{Agent, AgentRole, Agents};
use crate::audit::Audit;
use crate::auth::{AuthResource, AuthSubject, AuthVerb};
use crate::primitives::{AgentId, SandboxId};
use crate::sandbox::SandboxAgentMode;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::parse_params;

#[derive(Deserialize, schemars::JsonSchema)]
struct ProjectAgentParams {
    command: ProjectAgentCommand,
    name: Option<String>,
    #[schemars(with = "Option<uuid::Uuid>")]
    agent_id: Option<AgentId>,
    #[schemars(with = "Option<uuid::Uuid>")]
    sandbox_id: Option<SandboxId>,
    mode: Option<SandboxAgentMode>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ProjectAgentCommand {
    Create,
    List,
    AttachSandbox,
    DetachSandbox,
    Delete,
}

impl ProjectAgentCommand {
    fn audit_action(&self) -> &'static str {
        match self {
            Self::Create => "agent.create",
            Self::List => "agent.list",
            Self::AttachSandbox => "agent.attach_sandbox",
            Self::DetachSandbox => "agent.detach_sandbox",
            Self::Delete => "agent.delete",
        }
    }
}

pub struct ProjectAgent {
    agents: Arc<Agents>,
}

impl ProjectAgent {
    pub fn new(agents: Arc<Agents>) -> Self {
        Self { agents }
    }
}

static SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    let settings = schemars::gen::SchemaSettings::draft07().with(|s| {
        s.inline_subschemas = true;
        s.meta_schema = None;
    });
    let generator = settings.into_generator();
    let schema = generator.into_root_schema_for::<ProjectAgentParams>();
    let mut value = serde_json::to_value(schema).expect("schema serialization");
    if let Some(obj) = value.as_object_mut() {
        obj.remove("title");
        // `definitions` retained for $ref resolution by the compose TS generator.
        obj.insert(
            "additionalProperties".into(),
            serde_json::Value::Bool(false),
        );
    }
    value
});

#[async_trait::async_trait]
impl TopLevelTool for ProjectAgent {
    fn name(&self) -> &str {
        "agent"
    }

    fn description(&self) -> &str {
        "Manage agents. Commands: `create` (requires `name`), `list`, \
         `attach_sandbox` (requires `agent_id`, `sandbox_id`, optional `mode`), \
         `detach_sandbox` (requires `agent_id`, `sandbox_id`), \
         `delete` (requires `agent_id`; soft-deletes the agent, cascades \
         to its session and detaches any attached sandbox)."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.project_id().is_some_and(|project| {
            subject
                .can(AuthVerb::Read, AuthResource::Agent(project, None))
                .is_ok()
        })
    }

    fn composable(&self) -> bool {
        true
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let project_id = subject.project_id().ok_or(ToolSetsError::Unauthorized)?;
        let params: ProjectAgentParams = parse_params(arguments)?;

        Audit::record_action(params.command.audit_action());

        match params.command {
            ProjectAgentCommand::Create => {
                let name = params.name.ok_or_else(|| {
                    ToolSetsError::MissingArgument("name is required for create".to_string())
                })?;
                let agent = self
                    .agents
                    .create_agent(subject, project_id, &name, None, None)
                    .await
                    .map_err(|e| ToolSetsError::Agent(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(format_agent(
                    &agent,
                ))]))
            }

            ProjectAgentCommand::List => {
                let agents = self
                    .agents
                    .list_for_project(subject, project_id)
                    .await
                    .map_err(|e| ToolSetsError::Agent(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(format_agents(
                    &agents,
                ))]))
            }

            ProjectAgentCommand::AttachSandbox => {
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

                let agent = self
                    .agents
                    .attach_sandbox(subject, agent_id, sandbox_id, mode)
                    .await
                    .map_err(|e| ToolSetsError::Agent(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(format_agent(
                    &agent,
                ))]))
            }

            ProjectAgentCommand::DetachSandbox => {
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

                let agent = self
                    .agents
                    .detach_sandbox(subject, agent_id, sandbox_id)
                    .await
                    .map_err(|e| ToolSetsError::Agent(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(format_agent(
                    &agent,
                ))]))
            }

            ProjectAgentCommand::Delete => {
                let agent_id = params.agent_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("agent_id is required for delete".to_string())
                })?;

                self.agents
                    .delete(subject, agent_id)
                    .await
                    .map_err(|e| ToolSetsError::Agent(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "Agent deleted (id {agent_id})."
                ))]))
            }
        }
    }
}

fn format_agent(a: &Agent) -> String {
    let role = match a.agent_role {
        AgentRole::ProjectLead => "project_lead",
        AgentRole::Agent => "agent",
        AgentRole::WorkflowStepAgent => "workflow_step_agent",
    };
    let sandbox = match &a.attached_sandbox {
        Some((sid, mode)) => format!("{sid} ({mode:?})"),
        None => "none".to_string(),
    };
    format!(
        "Agent created.\n  id: {}\n  name: {}\n  role: {}\n  project: {}\n  sandbox: {}",
        a.id, a.name, role, a.project_id, sandbox
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
            AgentRole::ProjectLead => "project_lead",
            AgentRole::Agent => "agent",
            AgentRole::WorkflowStepAgent => "workflow_step_agent",
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
