//! Sandbox management tools: workspace-scoped and admin-scoped wrappers for
//! creating, listing, and inspecting sandboxes.
//!
//! Workspace-scoped tools require `WorkspaceWrite` (or `WorkspaceRead` for
//! listing/get) on the caller's workspace.  Admin-scoped tools require the
//! `Admin` scope.

use std::sync::LazyLock;

use rmcp::model::{CallToolResult, Content, JsonObject};

use crate::auth::AuthSubject;
use crate::primitives::{AuthScope, SandboxId, WorkspaceId};
use crate::sandbox::{Sandbox, Sandboxes};

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn can_write_workspace(subject: &AuthSubject) -> bool {
    subject
        .workspace_id()
        .is_some_and(|ws| subject.has_scope(&AuthScope::WorkspaceWrite(ws)))
}

fn can_read_workspace(subject: &AuthSubject) -> bool {
    subject
        .workspace_id()
        .is_some_and(|ws| subject.has_scope(&AuthScope::WorkspaceRead(ws)))
}

fn parse_uuid_field(args: Option<&JsonObject>, key: &str) -> Option<uuid::Uuid> {
    args.and_then(|a| a.get(key))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<uuid::Uuid>().ok())
}

fn require_uuid_field(args: Option<&JsonObject>, key: &str) -> Result<uuid::Uuid, ToolSetsError> {
    parse_uuid_field(args, key).ok_or_else(|| ToolSetsError::MissingArgument(key.to_string()))
}

/// Format a list of sandboxes as a human-/LLM-readable text table.
fn format_sandboxes(sandboxes: &[Sandbox]) -> String {
    if sandboxes.is_empty() {
        return "No sandboxes found.".to_string();
    }

    let mut lines = Vec::with_capacity(sandboxes.len() + 2);
    lines.push(format!(
        "{:<38} {:<20} {:<14} {:<10} {:<8}",
        "ID", "NAME", "STATE", "MODE", "AGENTS"
    ));
    lines.push("-".repeat(94));

    for s in sandboxes {
        let mode = format!("{:?}", s.mode);
        lines.push(format!(
            "{:<38} {:<20} {:<14} {:<10} {:<8}",
            s.id,
            truncate(&s.name, 20),
            s.state.to_string(),
            truncate(&mode, 10),
            s.attached_agents.len(),
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

/// Format a single sandbox as a detailed text block.
fn format_sandbox(s: &Sandbox) -> String {
    let agents: Vec<String> = s
        .attached_agents
        .iter()
        .map(|(id, mode)| format!("  {id} ({mode:?})"))
        .collect();
    let agents_str = if agents.is_empty() {
        "  none".to_string()
    } else {
        agents.join("\n")
    };
    let error_str = s
        .last_error
        .as_deref()
        .map(|e| format!("\n  last_error: {e}"))
        .unwrap_or_default();
    format!(
        "Sandbox:\n  id: {}\n  name: {}\n  workspace: {}\n  state: {}\n  mode: {:?}\n  specs: cpu={}, mem={}, disk={}{}\n  attached_agents:\n{}",
        s.id, s.name, s.workspace_id, s.state, s.mode,
        s.specs.cpu, s.specs.memory, s.specs.disk_size,
        error_str, agents_str,
    )
}

// ---------------------------------------------------------------------------
// workspace_create_sandbox
// ---------------------------------------------------------------------------

pub struct WorkspaceCreateSandbox {
    sandboxes: Sandboxes,
}

impl WorkspaceCreateSandbox {
    pub fn new(sandboxes: Sandboxes) -> Self {
        Self { sandboxes }
    }
}

static WS_CREATE_SANDBOX_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "Display name for the new sandbox."
            },
            "mode": {
                "type": "string",
                "enum": ["scratch", "repo"],
                "description": "Sandbox mode. 'scratch' for empty workspace, 'repo' to clone a repository."
            },
            "repo_url": {
                "type": "string",
                "description": "Repository URL to clone (required when mode is 'repo')."
            },
            "cpu": {
                "type": "string",
                "description": "CPU resource spec (e.g. '500m'). Defaults to '500m'."
            },
            "memory": {
                "type": "string",
                "description": "Memory resource spec (e.g. '512Mi'). Defaults to '512Mi'."
            },
            "disk_size": {
                "type": "string",
                "description": "Disk size spec (e.g. '10Gi'). Defaults to '10Gi'."
            }
        },
        "required": ["name", "mode"],
        "additionalProperties": false,
    })
});

#[async_trait::async_trait]
impl TopLevelTool for WorkspaceCreateSandbox {
    fn name(&self) -> &str {
        "workspace_create_sandbox"
    }

    fn description(&self) -> &str {
        "Create a new sandbox in the caller's workspace."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &WS_CREATE_SANDBOX_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        can_write_workspace(subject)
    }

    fn can_execute(&self, subject: &AuthSubject) -> bool {
        can_write_workspace(subject)
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let workspace_id = subject.workspace_id().ok_or(ToolSetsError::Unauthorized)?;
        let args = arguments.as_ref();
        let (name, specs, mode) = parse_sandbox_create_args(args)?;

        let sandbox = self
            .sandboxes
            .create(workspace_id, name, specs, mode)
            .await
            .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;

        Ok(CallToolResult::success(vec![Content::text(
            format_sandbox(&sandbox),
        )]))
    }
}

// ---------------------------------------------------------------------------
// create_sandbox (admin)
// ---------------------------------------------------------------------------

pub struct CreateSandbox {
    sandboxes: Sandboxes,
}

impl CreateSandbox {
    pub fn new(sandboxes: Sandboxes) -> Self {
        Self { sandboxes }
    }
}

static ADMIN_CREATE_SANDBOX_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "workspace_id": {
                "type": "string",
                "format": "uuid",
                "description": "Workspace to create the sandbox in."
            },
            "name": {
                "type": "string",
                "description": "Display name for the new sandbox."
            },
            "mode": {
                "type": "string",
                "enum": ["scratch", "repo"],
                "description": "Sandbox mode. 'scratch' for empty workspace, 'repo' to clone a repository."
            },
            "repo_url": {
                "type": "string",
                "description": "Repository URL to clone (required when mode is 'repo')."
            },
            "cpu": {
                "type": "string",
                "description": "CPU resource spec (e.g. '500m'). Defaults to '500m'."
            },
            "memory": {
                "type": "string",
                "description": "Memory resource spec (e.g. '512Mi'). Defaults to '512Mi'."
            },
            "disk_size": {
                "type": "string",
                "description": "Disk size spec (e.g. '10Gi'). Defaults to '10Gi'."
            }
        },
        "required": ["workspace_id", "name", "mode"],
        "additionalProperties": false,
    })
});

#[async_trait::async_trait]
impl TopLevelTool for CreateSandbox {
    fn name(&self) -> &str {
        "create_sandbox"
    }

    fn description(&self) -> &str {
        "Create a new sandbox in a specified workspace (admin)."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &ADMIN_CREATE_SANDBOX_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.has_scope(&AuthScope::Admin)
    }

    fn can_execute(&self, subject: &AuthSubject) -> bool {
        subject.has_scope(&AuthScope::Admin)
    }

    async fn call(
        &self,
        _subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let args = arguments.as_ref();
        let workspace_id: WorkspaceId = require_uuid_field(args, "workspace_id")?.into();
        let (name, specs, mode) = parse_sandbox_create_args(args)?;

        let sandbox = self
            .sandboxes
            .create(workspace_id, name, specs, mode)
            .await
            .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;

        Ok(CallToolResult::success(vec![Content::text(
            format_sandbox(&sandbox),
        )]))
    }
}

// ---------------------------------------------------------------------------
// workspace_list_sandboxes
// ---------------------------------------------------------------------------

pub struct WorkspaceListSandboxes {
    sandboxes: Sandboxes,
}

impl WorkspaceListSandboxes {
    pub fn new(sandboxes: Sandboxes) -> Self {
        Self { sandboxes }
    }
}

static WS_LIST_SANDBOXES_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false,
    })
});

#[async_trait::async_trait]
impl TopLevelTool for WorkspaceListSandboxes {
    fn name(&self) -> &str {
        "workspace_list_sandboxes"
    }

    fn description(&self) -> &str {
        "List all sandboxes in the caller's workspace."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &WS_LIST_SANDBOXES_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        can_read_workspace(subject)
    }

    fn can_execute(&self, subject: &AuthSubject) -> bool {
        can_read_workspace(subject)
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        _arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let workspace_id = subject.workspace_id().ok_or(ToolSetsError::Unauthorized)?;

        let sandboxes = self
            .sandboxes
            .list_for_workspace(workspace_id)
            .await
            .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;

        Ok(CallToolResult::success(vec![Content::text(
            format_sandboxes(&sandboxes),
        )]))
    }
}

// ---------------------------------------------------------------------------
// list_sandboxes (admin)
// ---------------------------------------------------------------------------

pub struct ListSandboxes {
    sandboxes: Sandboxes,
}

impl ListSandboxes {
    pub fn new(sandboxes: Sandboxes) -> Self {
        Self { sandboxes }
    }
}

static ADMIN_LIST_SANDBOXES_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "workspace_id": {
                "type": "string",
                "format": "uuid",
                "description": "Workspace to list sandboxes for."
            }
        },
        "required": ["workspace_id"],
        "additionalProperties": false,
    })
});

#[async_trait::async_trait]
impl TopLevelTool for ListSandboxes {
    fn name(&self) -> &str {
        "list_sandboxes"
    }

    fn description(&self) -> &str {
        "List all sandboxes in a workspace (admin)."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &ADMIN_LIST_SANDBOXES_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.has_scope(&AuthScope::Admin)
    }

    fn can_execute(&self, subject: &AuthSubject) -> bool {
        subject.has_scope(&AuthScope::Admin)
    }

    async fn call(
        &self,
        _subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let args = arguments.as_ref();
        let workspace_id: WorkspaceId = require_uuid_field(args, "workspace_id")?.into();

        let sandboxes = self
            .sandboxes
            .list_for_workspace(workspace_id)
            .await
            .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;

        Ok(CallToolResult::success(vec![Content::text(
            format_sandboxes(&sandboxes),
        )]))
    }
}

// ---------------------------------------------------------------------------
// workspace_get_sandbox
// ---------------------------------------------------------------------------

pub struct WorkspaceGetSandbox {
    sandboxes: Sandboxes,
}

impl WorkspaceGetSandbox {
    pub fn new(sandboxes: Sandboxes) -> Self {
        Self { sandboxes }
    }
}

static WS_GET_SANDBOX_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "sandbox_id": {
                "type": "string",
                "format": "uuid",
                "description": "ID of the sandbox to retrieve."
            }
        },
        "required": ["sandbox_id"],
        "additionalProperties": false,
    })
});

#[async_trait::async_trait]
impl TopLevelTool for WorkspaceGetSandbox {
    fn name(&self) -> &str {
        "workspace_get_sandbox"
    }

    fn description(&self) -> &str {
        "Get sandbox details by ID (workspace-scoped)."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &WS_GET_SANDBOX_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        can_read_workspace(subject)
    }

    fn can_execute(&self, subject: &AuthSubject) -> bool {
        can_read_workspace(subject)
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let workspace_id = subject.workspace_id().ok_or(ToolSetsError::Unauthorized)?;
        let args = arguments.as_ref();
        let sandbox_id: SandboxId = require_uuid_field(args, "sandbox_id")?.into();

        let sandbox = self
            .sandboxes
            .find_by_id(sandbox_id)
            .await
            .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;

        // Verify the sandbox belongs to the caller's workspace.
        if sandbox.workspace_id != workspace_id {
            return Err(ToolSetsError::Unauthorized);
        }

        Ok(CallToolResult::success(vec![Content::text(
            format_sandbox(&sandbox),
        )]))
    }
}

// ---------------------------------------------------------------------------
// get_sandbox (admin)
// ---------------------------------------------------------------------------

pub struct GetSandbox {
    sandboxes: Sandboxes,
}

impl GetSandbox {
    pub fn new(sandboxes: Sandboxes) -> Self {
        Self { sandboxes }
    }
}

// Re-uses WS_GET_SANDBOX_SCHEMA — same field.

#[async_trait::async_trait]
impl TopLevelTool for GetSandbox {
    fn name(&self) -> &str {
        "get_sandbox"
    }

    fn description(&self) -> &str {
        "Get sandbox details by ID (admin)."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &WS_GET_SANDBOX_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.has_scope(&AuthScope::Admin)
    }

    fn can_execute(&self, subject: &AuthSubject) -> bool {
        subject.has_scope(&AuthScope::Admin)
    }

    async fn call(
        &self,
        _subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let args = arguments.as_ref();
        let sandbox_id: SandboxId = require_uuid_field(args, "sandbox_id")?.into();

        let sandbox = self
            .sandboxes
            .find_by_id(sandbox_id)
            .await
            .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;

        Ok(CallToolResult::success(vec![Content::text(
            format_sandbox(&sandbox),
        )]))
    }
}

// ---------------------------------------------------------------------------
// Shared create-args parser
// ---------------------------------------------------------------------------

fn parse_sandbox_create_args(
    args: Option<&JsonObject>,
) -> Result<(String, sandbox::SandboxSpecs, sandbox::SandboxMode), ToolSetsError> {
    let name = args
        .and_then(|a| a.get("name"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolSetsError::MissingArgument("name".to_string()))?
        .to_string();

    let mode_str = args
        .and_then(|a| a.get("mode"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolSetsError::MissingArgument("mode".to_string()))?;

    let mode = match mode_str {
        "repo" => {
            let repo_url = args
                .and_then(|a| a.get("repo_url"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    ToolSetsError::MissingArgument("repo_url (required for mode=repo)".to_string())
                })?
                .to_string();
            sandbox::SandboxMode::Repo { repo_url }
        }
        _ => sandbox::SandboxMode::Scratch,
    };

    let cpu = args
        .and_then(|a| a.get("cpu"))
        .and_then(|v| v.as_str())
        .unwrap_or("500m")
        .to_string();
    let memory = args
        .and_then(|a| a.get("memory"))
        .and_then(|v| v.as_str())
        .unwrap_or("512Mi")
        .to_string();
    let disk_size = args
        .and_then(|a| a.get("disk_size"))
        .and_then(|v| v.as_str())
        .unwrap_or("10Gi")
        .to_string();

    let specs = sandbox::SandboxSpecs {
        cpu,
        memory,
        disk_size,
    };

    Ok((name, specs, mode))
}
