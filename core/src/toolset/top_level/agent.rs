//! Agent management tools: workspace-scoped and admin-scoped wrappers for
//! creating agents, listing agents, and attaching/detaching sandboxes.
//!
//! Workspace-scoped tools require `WorkspaceWrite` (or `WorkspaceRead` for
//! listing) on the caller's workspace.  Admin-scoped tools require the
//! `Admin` scope.

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};

use crate::agent::{Agent, AgentRole, Agents};
use crate::auth::AuthSubject;
use crate::primitives::{AgentId, SandboxId, WorkspaceId};
use crate::sandbox::SandboxAgentMode;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;

// ---------------------------------------------------------------------------
// workspace_create_agent
// ---------------------------------------------------------------------------

pub struct WorkspaceAgentCreate {
    agents: Arc<Agents>,
}

impl WorkspaceAgentCreate {
    pub fn new(agents: Arc<Agents>) -> Self {
        Self { agents }
    }
}

static WS_AGENT_CREATE_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "Display name for the new agent."
            }
        },
        "required": ["name"],
        "additionalProperties": false,
    })
});

#[async_trait::async_trait]
impl TopLevelTool for WorkspaceAgentCreate {
    fn name(&self) -> &str {
        "workspace_create_agent"
    }

    fn description(&self) -> &str {
        "Create a new agent in the caller's workspace."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &WS_AGENT_CREATE_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.can_write_workspace()
    }

    fn can_execute(&self, subject: &AuthSubject) -> bool {
        subject.can_write_workspace()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let workspace_id = subject.workspace_id().ok_or(ToolSetsError::Unauthorized)?;
        let args = arguments.as_ref();

        let name = args
            .and_then(|a| a.get("name"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolSetsError::MissingArgument("name".to_string()))?;

        let agent = self
            .agents
            .create(workspace_id, AgentRole::Agent, name, None)
            .await
            .map_err(|e| ToolSetsError::Agent(e.to_string()))?;

        Ok(CallToolResult::success(vec![Content::text(format_agent(
            &agent,
        ))]))
    }
}

// ---------------------------------------------------------------------------
// create_agent (admin)
// ---------------------------------------------------------------------------

pub struct AgentCreate {
    agents: Arc<Agents>,
}

impl AgentCreate {
    pub fn new(agents: Arc<Agents>) -> Self {
        Self { agents }
    }
}

static ADMIN_AGENT_CREATE_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "workspace_id": {
                "type": "string",
                "format": "uuid",
                "description": "Workspace to create the agent in."
            },
            "name": {
                "type": "string",
                "description": "Display name for the new agent."
            }
        },
        "required": ["workspace_id", "name"],
        "additionalProperties": false,
    })
});

#[async_trait::async_trait]
impl TopLevelTool for AgentCreate {
    fn name(&self) -> &str {
        "create_agent"
    }

    fn description(&self) -> &str {
        "Create a new agent in a specified workspace (admin)."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &ADMIN_AGENT_CREATE_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.is_admin()
    }

    fn can_execute(&self, subject: &AuthSubject) -> bool {
        subject.is_admin()
    }

    async fn call(
        &self,
        _subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let args = arguments.as_ref();
        let workspace_id: WorkspaceId = require_uuid_field(args, "workspace_id")?.into();
        let name = args
            .and_then(|a| a.get("name"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolSetsError::MissingArgument("name".to_string()))?;

        let agent = self
            .agents
            .create(workspace_id, AgentRole::Agent, name, None)
            .await
            .map_err(|e| ToolSetsError::Agent(e.to_string()))?;

        Ok(CallToolResult::success(vec![Content::text(format_agent(
            &agent,
        ))]))
    }
}

// ---------------------------------------------------------------------------
// workspace_attach_sandbox
// ---------------------------------------------------------------------------

pub struct WorkspaceAgentAttachSandbox {
    agents: Arc<Agents>,
}

impl WorkspaceAgentAttachSandbox {
    pub fn new(agents: Arc<Agents>) -> Self {
        Self { agents }
    }
}

static WS_ATTACH_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "agent_id": {
                "type": "string",
                "format": "uuid",
                "description": "ID of the agent to attach the sandbox to."
            },
            "sandbox_id": {
                "type": "string",
                "format": "uuid",
                "description": "ID of the sandbox to attach."
            },
            "mode": {
                "type": "string",
                "enum": ["read", "write"],
                "description": "Attach mode. Defaults to 'read'."
            }
        },
        "required": ["agent_id", "sandbox_id"],
        "additionalProperties": false,
    })
});

#[async_trait::async_trait]
impl TopLevelTool for WorkspaceAgentAttachSandbox {
    fn name(&self) -> &str {
        "workspace_attach_sandbox"
    }

    fn description(&self) -> &str {
        "Attach a sandbox to an agent in the caller's workspace."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &WS_ATTACH_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.can_write_workspace()
    }

    fn can_execute(&self, subject: &AuthSubject) -> bool {
        subject.can_write_workspace()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let args = arguments.as_ref();
        let agent_id: AgentId = require_uuid_field(args, "agent_id")?.into();
        let sandbox_id: SandboxId = require_uuid_field(args, "sandbox_id")?.into();
        let mode = parse_sandbox_mode(args);

        let agent = self
            .agents
            .attach_sandbox(subject, agent_id, sandbox_id, mode)
            .await
            .map_err(|e| ToolSetsError::Agent(e.to_string()))?;

        Ok(CallToolResult::success(vec![Content::text(format_agent(
            &agent,
        ))]))
    }
}

// ---------------------------------------------------------------------------
// attach_sandbox (admin)
// ---------------------------------------------------------------------------

pub struct AgentAttachSandbox {
    agents: Arc<Agents>,
}

impl AgentAttachSandbox {
    pub fn new(agents: Arc<Agents>) -> Self {
        Self { agents }
    }
}

static ADMIN_ATTACH_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "agent_id": {
                "type": "string",
                "format": "uuid",
                "description": "ID of the agent to attach the sandbox to."
            },
            "sandbox_id": {
                "type": "string",
                "format": "uuid",
                "description": "ID of the sandbox to attach."
            },
            "mode": {
                "type": "string",
                "enum": ["read", "write"],
                "description": "Attach mode. Defaults to 'read'."
            }
        },
        "required": ["agent_id", "sandbox_id"],
        "additionalProperties": false,
    })
});

#[async_trait::async_trait]
impl TopLevelTool for AgentAttachSandbox {
    fn name(&self) -> &str {
        "attach_sandbox"
    }

    fn description(&self) -> &str {
        "Attach a sandbox to an agent (admin)."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &ADMIN_ATTACH_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.is_admin()
    }

    fn can_execute(&self, subject: &AuthSubject) -> bool {
        subject.is_admin()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let args = arguments.as_ref();
        let agent_id: AgentId = require_uuid_field(args, "agent_id")?.into();
        let sandbox_id: SandboxId = require_uuid_field(args, "sandbox_id")?.into();
        let mode = args
            .and_then(|a| a.get("mode"))
            .and_then(|v| v.as_str())
            .map(|s| match s {
                "write" => SandboxAgentMode::Write,
                _ => SandboxAgentMode::Read,
            })
            .unwrap_or(SandboxAgentMode::Read);

        let agent = self
            .agents
            .attach_sandbox(subject, agent_id, sandbox_id, mode)
            .await
            .map_err(|e| ToolSetsError::Agent(e.to_string()))?;

        Ok(CallToolResult::success(vec![Content::text(format_agent(
            &agent,
        ))]))
    }
}

// ---------------------------------------------------------------------------
// workspace_detach_sandbox
// ---------------------------------------------------------------------------

pub struct WorkspaceAgentDetachSandbox {
    agents: Arc<Agents>,
}

impl WorkspaceAgentDetachSandbox {
    pub fn new(agents: Arc<Agents>) -> Self {
        Self { agents }
    }
}

static WS_DETACH_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "agent_id": {
                "type": "string",
                "format": "uuid",
                "description": "ID of the agent to detach the sandbox from."
            },
            "sandbox_id": {
                "type": "string",
                "format": "uuid",
                "description": "ID of the sandbox to detach."
            }
        },
        "required": ["agent_id", "sandbox_id"],
        "additionalProperties": false,
    })
});

#[async_trait::async_trait]
impl TopLevelTool for WorkspaceAgentDetachSandbox {
    fn name(&self) -> &str {
        "workspace_detach_sandbox"
    }

    fn description(&self) -> &str {
        "Detach a sandbox from an agent in the caller's workspace."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &WS_DETACH_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.can_write_workspace()
    }

    fn can_execute(&self, subject: &AuthSubject) -> bool {
        subject.can_write_workspace()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let workspace_id = subject.workspace_id().ok_or(ToolSetsError::Unauthorized)?;
        let args = arguments.as_ref();
        let agent_id: AgentId = require_uuid_field(args, "agent_id")?.into();
        let sandbox_id: SandboxId = require_uuid_field(args, "sandbox_id")?.into();

        let existing = self
            .agents
            .find_by_id(agent_id)
            .await
            .map_err(|e| ToolSetsError::Agent(e.to_string()))?;
        if existing.workspace_id != workspace_id {
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

// ---------------------------------------------------------------------------
// detach_sandbox (admin)
// ---------------------------------------------------------------------------

pub struct AgentDetachSandbox {
    agents: Arc<Agents>,
}

impl AgentDetachSandbox {
    pub fn new(agents: Arc<Agents>) -> Self {
        Self { agents }
    }
}

#[async_trait::async_trait]
impl TopLevelTool for AgentDetachSandbox {
    fn name(&self) -> &str {
        "detach_sandbox"
    }

    fn description(&self) -> &str {
        "Detach a sandbox from an agent (admin)."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &WS_DETACH_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.is_admin()
    }

    fn can_execute(&self, subject: &AuthSubject) -> bool {
        subject.is_admin()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let args = arguments.as_ref();
        let agent_id: AgentId = require_uuid_field(args, "agent_id")?.into();
        let sandbox_id: SandboxId = require_uuid_field(args, "sandbox_id")?.into();

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

// ---------------------------------------------------------------------------
// workspace_list_agents
// ---------------------------------------------------------------------------

pub struct WorkspaceListAgents {
    agents: Arc<Agents>,
}

impl WorkspaceListAgents {
    pub fn new(agents: Arc<Agents>) -> Self {
        Self { agents }
    }
}

static WS_LIST_AGENTS_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false,
    })
});

#[async_trait::async_trait]
impl TopLevelTool for WorkspaceListAgents {
    fn name(&self) -> &str {
        "workspace_list_agents"
    }

    fn description(&self) -> &str {
        "List all agents in the caller's workspace."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &WS_LIST_AGENTS_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.can_read_workspace()
    }

    fn can_execute(&self, subject: &AuthSubject) -> bool {
        subject.can_read_workspace()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        _arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let workspace_id = subject.workspace_id().ok_or(ToolSetsError::Unauthorized)?;

        let agents = self
            .agents
            .list_for_workspace(workspace_id)
            .await
            .map_err(|e| ToolSetsError::Agent(e.to_string()))?;

        Ok(CallToolResult::success(vec![Content::text(format_agents(
            &agents,
        ))]))
    }
}

// ---------------------------------------------------------------------------
// list_agents (admin)
// ---------------------------------------------------------------------------

pub struct ListAgents {
    agents: Arc<Agents>,
}

impl ListAgents {
    pub fn new(agents: Arc<Agents>) -> Self {
        Self { agents }
    }
}

static ADMIN_LIST_AGENTS_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "workspace_id": {
                "type": "string",
                "format": "uuid",
                "description": "Workspace to list agents for."
            }
        },
        "required": ["workspace_id"],
        "additionalProperties": false,
    })
});

#[async_trait::async_trait]
impl TopLevelTool for ListAgents {
    fn name(&self) -> &str {
        "list_agents"
    }

    fn description(&self) -> &str {
        "List all agents in a workspace (admin)."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &ADMIN_LIST_AGENTS_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.is_admin()
    }

    fn can_execute(&self, subject: &AuthSubject) -> bool {
        subject.is_admin()
    }

    async fn call(
        &self,
        _subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let args = arguments.as_ref();
        let workspace_id: WorkspaceId = require_uuid_field(args, "workspace_id")?.into();

        let agents = self
            .agents
            .list_for_workspace(workspace_id)
            .await
            .map_err(|e| ToolSetsError::Agent(e.to_string()))?;

        Ok(CallToolResult::success(vec![Content::text(format_agents(
            &agents,
        ))]))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn require_uuid_field(args: Option<&JsonObject>, key: &str) -> Result<uuid::Uuid, ToolSetsError> {
    args.and_then(|a| a.get(key))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<uuid::Uuid>().ok())
        .ok_or_else(|| ToolSetsError::MissingArgument(key.to_string()))
}

fn parse_sandbox_mode(args: Option<&JsonObject>) -> SandboxAgentMode {
    args.and_then(|a| a.get("mode"))
        .and_then(|v| v.as_str())
        .map(|s| match s {
            "write" => SandboxAgentMode::Write,
            _ => SandboxAgentMode::Read,
        })
        .unwrap_or(SandboxAgentMode::Read)
}

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
