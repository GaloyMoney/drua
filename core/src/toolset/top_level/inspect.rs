//! Sandbox inspection tools: read-only access to any sandbox's filesystem.
//!
//! `workspace_inspect_sandbox` lets a workspace reader run read-only tools
//! (grep, glob, read, ls) against any sandbox within their workspace.
//! `inspect_sandbox` is the admin variant with no workspace constraint.

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use sandbox::instance_client::ExecuteRequest;
use serde::Deserialize;

use crate::auth::AuthSubject;
use crate::primitives::SandboxId;
use crate::sandbox::Sandboxes;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{parse_params, schema_for};

// ---------------------------------------------------------------------------
// Params
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum InspectTool {
    /// Search file contents.
    Grep,
    /// Find files by pattern.
    Glob,
    /// Read file contents.
    Read,
    /// List directory entries.
    Ls,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct InspectParams {
    /// ID of the sandbox to inspect.
    #[schemars(with = "uuid::Uuid")]
    sandbox_id: SandboxId,
    /// Read-only tool to run against the sandbox.
    tool: InspectTool,
    /// Tool-specific arguments passed through to the sandbox. grep: {pattern, path?, glob?, output_mode?, ...}. glob: {pattern, path?}. read: {path, offset?, limit?}. ls: {path, ignore?}.
    #[serde(default)]
    arguments: JsonObject,
}

// ---------------------------------------------------------------------------
// workspace_inspect_sandbox
// ---------------------------------------------------------------------------

pub struct WorkspaceInspectSandbox {
    sandboxes: Arc<Sandboxes>,
}

impl WorkspaceInspectSandbox {
    pub fn new(sandboxes: Arc<Sandboxes>) -> Self {
        Self { sandboxes }
    }
}

static WS_INSPECT_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<InspectParams>);

#[async_trait::async_trait]
impl TopLevelTool for WorkspaceInspectSandbox {
    fn name(&self) -> &str {
        "workspace_inspect_sandbox"
    }

    fn description(&self) -> &str {
        "Run a read-only tool (grep, glob, read, ls) against any sandbox \
         in the caller's workspace."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &WS_INSPECT_SCHEMA
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
        let params: InspectParams = parse_params(arguments)?;

        let sandbox = self
            .sandboxes
            .find_by_id(params.sandbox_id)
            .await
            .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;
        if sandbox.workspace_id != workspace_id {
            return Err(ToolSetsError::Unauthorized);
        }

        execute_inspect(&self.sandboxes, params).await
    }
}

// ---------------------------------------------------------------------------
// inspect_sandbox (admin)
// ---------------------------------------------------------------------------

pub struct AdminInspectSandbox {
    sandboxes: Arc<Sandboxes>,
}

impl AdminInspectSandbox {
    pub fn new(sandboxes: Arc<Sandboxes>) -> Self {
        Self { sandboxes }
    }
}

static ADMIN_INSPECT_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<InspectParams>);

#[async_trait::async_trait]
impl TopLevelTool for AdminInspectSandbox {
    fn name(&self) -> &str {
        "admin_inspect_sandbox"
    }

    fn description(&self) -> &str {
        "Run a read-only tool (grep, glob, read, ls) against any sandbox (admin)."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &ADMIN_INSPECT_SCHEMA
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.is_admin()
    }

    async fn call(
        &self,
        _subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: InspectParams = parse_params(arguments)?;

        execute_inspect(&self.sandboxes, params).await
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Shared execution logic for both workspace and admin inspect tools.
async fn execute_inspect(
    sandboxes: &Sandboxes,
    params: InspectParams,
) -> Result<CallToolResult, ToolSetsError> {
    let is_ls = matches!(params.tool, InspectTool::Ls);
    let tool_args = params.arguments;

    // Extract LS ignore list before the match moves tool_args.
    let ls_ignore: Vec<String> = if is_ls {
        tool_args
            .get("ignore")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let req = match params.tool {
        InspectTool::Grep => ExecuteRequest {
            tool: "Grep".to_string(),
            input: serde_json::Value::Object(tool_args),
        },
        InspectTool::Glob => ExecuteRequest {
            tool: "Glob".to_string(),
            input: serde_json::Value::Object(tool_args),
        },
        InspectTool::Read => build_read_request(&tool_args)?,
        InspectTool::Ls => build_ls_request(&tool_args)?,
    };

    let client = sandboxes
        .instance_client_for(params.sandbox_id)
        .await
        .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;

    match client.execute(&req).await {
        Ok(resp) => {
            let mut output = resp.output;

            // LS: apply client-side ignore filter
            if is_ls && !ls_ignore.is_empty() {
                output = output
                    .lines()
                    .filter(|line| {
                        let name = line.trim_end_matches('/');
                        !ls_ignore.iter().any(|ig| ig == name)
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
            }

            let content = vec![Content::text(output)];
            if resp.is_error {
                Ok(CallToolResult::error(content))
            } else {
                Ok(CallToolResult::success(content))
            }
        }
        Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
            "sandbox /execute call failed: {e}"
        ))])),
    }
}

/// Translate `{path, offset?, limit?}` into the text editor's `view` command.
fn build_read_request(args: &JsonObject) -> Result<ExecuteRequest, ToolSetsError> {
    let path = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolSetsError::MissingArgument("path".to_string()))?;

    let mut input = serde_json::json!({
        "command": "view",
        "path": path,
    });

    let offset = args
        .get("offset")
        .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()));
    let limit = args
        .get("limit")
        .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()));

    if offset.is_some() || limit.is_some() {
        let start = offset.unwrap_or(0) + 1; // 0-based → 1-based
        let end = match limit {
            Some(l) => start + l - 1,
            None => -1, // EOF
        };
        input["view_range"] = serde_json::json!([start, end]);
    }

    Ok(ExecuteRequest {
        tool: "str_replace_based_edit_tool".to_string(),
        input,
    })
}

/// Translate `{path, ignore?}` into the text editor's `view` command on a directory.
fn build_ls_request(args: &JsonObject) -> Result<ExecuteRequest, ToolSetsError> {
    let path = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolSetsError::MissingArgument("path".to_string()))?;

    Ok(ExecuteRequest {
        tool: "str_replace_based_edit_tool".to_string(),
        input: serde_json::json!({
            "command": "view",
            "path": path,
        }),
    })
}
