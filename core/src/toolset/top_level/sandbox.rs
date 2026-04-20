//! Sandbox management tools: workspace-scoped wrappers for creating, listing,
//! and inspecting sandboxes.
//!
//! Workspace-scoped tools require the `WorkspaceAdmin` scope on the
//! caller's workspace.  Admin-scoped tools live in
//! [`super::super::searchable::admin`].

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use serde::Deserialize;

use crate::auth::AuthSubject;
use crate::primitives::SandboxId;
use crate::sandbox::{Sandbox, Sandboxes};

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{parse_params, schema_for};

// ---------------------------------------------------------------------------
// Params structs
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum SandboxCreateMode {
    /// Empty workspace.
    Scratch,
    /// Clone a repository.
    Repo,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct CreateSandboxParams {
    /// Display name for the new sandbox.
    name: String,
    /// Sandbox mode. 'scratch' for empty workspace, 'repo' to clone a repository.
    mode: SandboxCreateMode,
    /// Repository URL to clone (required when mode is 'repo').
    repo_url: Option<String>,
    /// Git branch to check out after cloning (optional, defaults to the repo's default branch). Only used when mode is 'repo'.
    branch: Option<String>,
    /// CPU resource spec (e.g. '500m'). Defaults to '500m'.
    #[serde(default = "default_cpu")]
    cpu: String,
    /// Memory resource spec (e.g. '512Mi'). Defaults to '512Mi'.
    #[serde(default = "default_memory")]
    memory: String,
    /// Disk size spec (e.g. '10Gi'). Defaults to '10Gi'.
    #[serde(default = "default_disk_size")]
    disk_size: String,
}

impl CreateSandboxParams {
    fn into_sandbox_args(
        self,
    ) -> Result<(String, sandbox::SandboxSpecs, sandbox::SandboxMode), ToolSetsError> {
        let mode = match self.mode {
            SandboxCreateMode::Repo => {
                let repo_url = self.repo_url.ok_or_else(|| {
                    ToolSetsError::InvalidArgument(
                        "repo_url is required when mode is 'repo'".to_string(),
                    )
                })?;
                sandbox::SandboxMode::Repo {
                    repo_url,
                    branch: self.branch,
                }
            }
            SandboxCreateMode::Scratch => sandbox::SandboxMode::Scratch,
        };

        let specs = sandbox::SandboxSpecs {
            cpu: self.cpu,
            memory: self.memory,
            disk_size: self.disk_size,
        };

        Ok((self.name, specs, mode))
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
struct GetSandboxParams {
    /// ID of the sandbox to retrieve.
    #[schemars(with = "uuid::Uuid")]
    sandbox_id: SandboxId,
}

fn default_cpu() -> String {
    "500m".to_string()
}
fn default_memory() -> String {
    "512Mi".to_string()
}
fn default_disk_size() -> String {
    "10Gi".to_string()
}

// ---------------------------------------------------------------------------
// workspace_create_sandbox
// ---------------------------------------------------------------------------

pub struct WorkspaceCreateSandbox {
    sandboxes: Arc<Sandboxes>,
}

impl WorkspaceCreateSandbox {
    pub fn new(sandboxes: Arc<Sandboxes>) -> Self {
        Self { sandboxes }
    }
}

static WS_CREATE_SANDBOX_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<CreateSandboxParams>);

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
        subject.can_write_workspace()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let workspace_id = subject.workspace_id().ok_or(ToolSetsError::Unauthorized)?;
        let params: CreateSandboxParams = parse_params(arguments)?;
        let (name, specs, mode) = params.into_sandbox_args()?;

        let sandbox = self
            .sandboxes
            .create(subject, workspace_id, name, specs, mode)
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
    sandboxes: Arc<Sandboxes>,
}

impl WorkspaceListSandboxes {
    pub fn new(sandboxes: Arc<Sandboxes>) -> Self {
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
        subject.can_read_workspace()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        _arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let workspace_id = subject.workspace_id().ok_or(ToolSetsError::Unauthorized)?;

        let sandboxes = self
            .sandboxes
            .list_for_workspace(subject, workspace_id)
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
    sandboxes: Arc<Sandboxes>,
}

impl WorkspaceGetSandbox {
    pub fn new(sandboxes: Arc<Sandboxes>) -> Self {
        Self { sandboxes }
    }
}

static WS_GET_SANDBOX_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<GetSandboxParams>);

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
        subject.can_read_workspace()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let workspace_id = subject.workspace_id().ok_or(ToolSetsError::Unauthorized)?;
        let params: GetSandboxParams = parse_params(arguments)?;

        let sandbox = self
            .sandboxes
            .find_by_id(subject, params.sandbox_id)
            .await
            .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;

        if sandbox.workspace_id != workspace_id {
            return Err(ToolSetsError::Unauthorized);
        }

        Ok(CallToolResult::success(vec![Content::text(
            format_sandbox(&sandbox),
        )]))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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
