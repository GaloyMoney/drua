//! Workspace management tools (admin-scoped).
//!
//! Workspaces are created/listed via these tools when an operator wants to
//! provision them without going through the web UI (e.g. bootstrapping a
//! new org, scripted setup). Both gate on [`AuthSubject::is_admin`];
//! there's no workspace-scoped variant here — workspace creation is
//! above a workspace (chicken-and-egg).

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use serde::Deserialize;

use crate::auth::AuthSubject;
use crate::workspace::{Workspace, Workspaces};

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{parse_params, schema_for};

// ---------------------------------------------------------------------------
// Params
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
struct WorkspaceCreateParams {
    /// Display name for the new workspace.
    name: String,
    /// Optional freeform description.
    description: Option<String>,
}

impl WorkspaceCreateParams {
    /// Filter out empty descriptions so callers sending `""` get `None`.
    fn description(&self) -> Option<String> {
        self.description
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(String::from)
    }
}

// ---------------------------------------------------------------------------
// admin_create_workspace
// ---------------------------------------------------------------------------

pub struct AdminWorkspaceCreate {
    workspaces: Arc<Workspaces>,
}

impl AdminWorkspaceCreate {
    pub fn new(workspaces: Arc<Workspaces>) -> Self {
        Self { workspaces }
    }
}

static ADMIN_WORKSPACE_CREATE_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<WorkspaceCreateParams>);

#[async_trait::async_trait]
impl TopLevelTool for AdminWorkspaceCreate {
    fn name(&self) -> &str {
        "admin_create_workspace"
    }

    fn description(&self) -> &str {
        "Create a new workspace (admin). Also seeds the workspace with a \
         `WorkspaceLead` agent named 'lead'."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &ADMIN_WORKSPACE_CREATE_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.is_admin()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: WorkspaceCreateParams = parse_params(arguments)?;

        let workspace = self
            .workspaces
            .create(subject, &params.name, params.description())
            .await
            .map_err(|e| ToolSetsError::Workspace(e.to_string()))?;

        Ok(CallToolResult::success(vec![Content::text(
            format_workspace_created(&workspace),
        )]))
    }
}

// ---------------------------------------------------------------------------
// admin_list_workspaces
// ---------------------------------------------------------------------------

pub struct AdminWorkspaceList {
    workspaces: Arc<Workspaces>,
}

impl AdminWorkspaceList {
    pub fn new(workspaces: Arc<Workspaces>) -> Self {
        Self { workspaces }
    }
}

static ADMIN_WORKSPACE_LIST_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false,
    })
});

#[async_trait::async_trait]
impl TopLevelTool for AdminWorkspaceList {
    fn name(&self) -> &str {
        "admin_list_workspaces"
    }

    fn description(&self) -> &str {
        "List all workspaces in the system (admin)."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &ADMIN_WORKSPACE_LIST_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.is_admin()
    }

    async fn call(
        &self,
        _subject: &AuthSubject,
        _arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let all = self
            .workspaces
            .list_all()
            .await
            .map_err(|e| ToolSetsError::Workspace(e.to_string()))?;

        Ok(CallToolResult::success(vec![Content::text(
            format_workspaces(&all),
        )]))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn format_workspace_created(w: &Workspace) -> String {
    let description = w.description.as_deref().unwrap_or("—");
    format!(
        "Workspace created.\n  id: {}\n  name: {}\n  description: {}",
        w.id, w.name, description
    )
}

fn format_workspaces(ws: &[Workspace]) -> String {
    if ws.is_empty() {
        return "No workspaces found.".to_string();
    }

    let mut lines = Vec::with_capacity(ws.len() + 2);
    lines.push(format!("{:<38} {:<30} {}", "ID", "NAME", "DESCRIPTION"));
    lines.push("-".repeat(100));
    for w in ws {
        let description = w.description.as_deref().unwrap_or("—");
        lines.push(format!("{:<38} {:<30} {}", w.id, w.name, description));
    }
    lines.join("\n")
}
