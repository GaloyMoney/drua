//! Agent management tools: workspace-scoped and admin-scoped wrappers for
//! creating agents, listing agents, and attaching/detaching sandboxes.
//!
//! Workspace-scoped tools require the `WorkspaceAdmin` scope on the
//! caller's workspace.  Admin-scoped tools require the `Admin` scope.

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use serde::Deserialize;

use crate::agent::{Agent, AgentRole, Agents};
use crate::auth::AuthSubject;
use crate::primitives::{AgentId, SandboxId, WorkspaceId};
use crate::sandbox::{SandboxAgentMode, Sandboxes};

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{parse_params, schema_for};

// ---------------------------------------------------------------------------
// Params structs
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
struct AgentCreateParams {
    /// Display name for the new agent.
    name: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct AdminAgentCreateParams {
    /// Workspace to create the agent in.
    #[schemars(with = "uuid::Uuid")]
    workspace_id: WorkspaceId,
    /// Display name for the new agent.
    name: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct AttachSandboxParams {
    /// ID of the agent to attach the sandbox to.
    #[schemars(with = "uuid::Uuid")]
    agent_id: AgentId,
    /// ID of the sandbox to attach.
    #[schemars(with = "uuid::Uuid")]
    sandbox_id: SandboxId,
    /// Attach mode. Defaults to 'read'.
    #[serde(default = "default_mode")]
    mode: SandboxAgentMode,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct DetachSandboxParams {
    /// ID of the agent to detach the sandbox from.
    #[schemars(with = "uuid::Uuid")]
    agent_id: AgentId,
    /// ID of the sandbox to detach.
    #[schemars(with = "uuid::Uuid")]
    sandbox_id: SandboxId,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct AdminListAgentsParams {
    /// Workspace to list agents for.
    #[schemars(with = "uuid::Uuid")]
    workspace_id: WorkspaceId,
}

fn default_mode() -> SandboxAgentMode {
    SandboxAgentMode::Read
}

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

static WS_AGENT_CREATE_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<AgentCreateParams>);

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

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let workspace_id = subject.workspace_id().ok_or(ToolSetsError::Unauthorized)?;
        let params: AgentCreateParams = parse_params(arguments)?;

        let agent = self
            .agents
            .create(subject, workspace_id, AgentRole::Agent, &params.name, None)
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

pub struct AdminAgentCreate {
    agents: Arc<Agents>,
}

impl AdminAgentCreate {
    pub fn new(agents: Arc<Agents>) -> Self {
        Self { agents }
    }
}

static ADMIN_AGENT_CREATE_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<AdminAgentCreateParams>);

#[async_trait::async_trait]
impl TopLevelTool for AdminAgentCreate {
    fn name(&self) -> &str {
        "admin_create_agent"
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

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: AdminAgentCreateParams = parse_params(arguments)?;

        let agent = self
            .agents
            .create(
                subject,
                params.workspace_id,
                AgentRole::Agent,
                &params.name,
                None,
            )
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
    sandboxes: Arc<Sandboxes>,
}

impl WorkspaceAgentAttachSandbox {
    pub fn new(agents: Arc<Agents>, sandboxes: Arc<Sandboxes>) -> Self {
        Self { agents, sandboxes }
    }
}

static WS_ATTACH_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<AttachSandboxParams>);

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

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let workspace_id = subject.workspace_id().ok_or(ToolSetsError::Unauthorized)?;
        let params: AttachSandboxParams = parse_params(arguments)?;

        let sandbox = self
            .sandboxes
            .find_by_id(subject, params.sandbox_id)
            .await
            .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;
        if sandbox.workspace_id != workspace_id {
            return Err(ToolSetsError::Unauthorized);
        }

        let agent = self
            .agents
            .attach_sandbox(subject, params.agent_id, params.sandbox_id, params.mode)
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

pub struct AdminAgentAttachSandbox {
    agents: Arc<Agents>,
}

impl AdminAgentAttachSandbox {
    pub fn new(agents: Arc<Agents>) -> Self {
        Self { agents }
    }
}

static ADMIN_ATTACH_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<AttachSandboxParams>);

#[async_trait::async_trait]
impl TopLevelTool for AdminAgentAttachSandbox {
    fn name(&self) -> &str {
        "admin_attach_sandbox"
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

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: AttachSandboxParams = parse_params(arguments)?;

        let agent = self
            .agents
            .attach_sandbox(subject, params.agent_id, params.sandbox_id, params.mode)
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
    sandboxes: Arc<Sandboxes>,
}

impl WorkspaceAgentDetachSandbox {
    pub fn new(agents: Arc<Agents>, sandboxes: Arc<Sandboxes>) -> Self {
        Self { agents, sandboxes }
    }
}

static WS_DETACH_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<DetachSandboxParams>);

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

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let workspace_id = subject.workspace_id().ok_or(ToolSetsError::Unauthorized)?;
        let params: DetachSandboxParams = parse_params(arguments)?;

        let existing = self
            .agents
            .find_by_id(subject, params.agent_id)
            .await
            .map_err(|e| ToolSetsError::Agent(e.to_string()))?;
        if existing.workspace_id != workspace_id {
            return Err(ToolSetsError::Unauthorized);
        }

        let sandbox = self
            .sandboxes
            .find_by_id(subject, params.sandbox_id)
            .await
            .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;
        if sandbox.workspace_id != workspace_id {
            return Err(ToolSetsError::Unauthorized);
        }

        let agent = self
            .agents
            .detach_sandbox(subject, params.agent_id, params.sandbox_id)
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

pub struct AdminAgentDetachSandbox {
    agents: Arc<Agents>,
}

impl AdminAgentDetachSandbox {
    pub fn new(agents: Arc<Agents>) -> Self {
        Self { agents }
    }
}

#[async_trait::async_trait]
impl TopLevelTool for AdminAgentDetachSandbox {
    fn name(&self) -> &str {
        "admin_detach_sandbox"
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

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: DetachSandboxParams = parse_params(arguments)?;

        let agent = self
            .agents
            .detach_sandbox(subject, params.agent_id, params.sandbox_id)
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

    async fn call(
        &self,
        subject: &AuthSubject,
        _arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let workspace_id = subject.workspace_id().ok_or(ToolSetsError::Unauthorized)?;

        let agents = self
            .agents
            .list_for_workspace(subject, workspace_id)
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

pub struct AdminListAgents {
    agents: Arc<Agents>,
}

impl AdminListAgents {
    pub fn new(agents: Arc<Agents>) -> Self {
        Self { agents }
    }
}

static ADMIN_LIST_AGENTS_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<AdminListAgentsParams>);

#[async_trait::async_trait]
impl TopLevelTool for AdminListAgents {
    fn name(&self) -> &str {
        "admin_list_agents"
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

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: AdminListAgentsParams = parse_params(arguments)?;

        let agents = self
            .agents
            .list_for_workspace(subject, params.workspace_id)
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
