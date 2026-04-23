//! `workspace_sandbox` — consolidated workspace-scoped sandbox management.
//!
//! Single tool with a `command` discriminator (like `text_editor`):
//! `create`, `list`, `get`, `inspect`.
//!
//! Read commands (`list`, `get`, `inspect`) require `can_read_workspace`;
//! write commands (`create`) enforce `can_write_workspace` inside `call()`.

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use sandbox::instance_client::ExecuteRequest;
use serde::Deserialize;

use crate::audit::Audit;
use crate::auth::AuthSubject;
use crate::primitives::SandboxId;
use crate::sandbox::{Sandbox, Sandboxes};

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::parse_params;

// ---------------------------------------------------------------------------
// Params
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum SandboxCommand {
    /// Create a new sandbox.
    Create,
    /// List all sandboxes in the workspace.
    List,
    /// Get sandbox details by ID.
    Get,
    /// Run a read-only tool (grep, glob, read, ls) against a sandbox.
    Inspect,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum SandboxCreateMode {
    /// Empty workspace.
    Scratch,
    /// Clone a repository.
    Repo,
}

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
struct WorkspaceSandboxParams {
    /// Which sandbox operation to perform.
    command: SandboxCommand,

    // -- create fields --
    /// Display name for the new sandbox (required for `create`).
    name: Option<String>,
    /// Sandbox mode: 'scratch' for empty workspace, 'repo' to clone a repository (required for `create`).
    mode: Option<SandboxCreateMode>,
    /// Repository URL to clone (required when mode is 'repo').
    repo_url: Option<String>,
    /// Git branch to check out after cloning (optional, defaults to the repo's default branch).
    branch: Option<String>,
    /// CPU resource spec (e.g. '500m'). Defaults to '500m'.
    cpu: Option<String>,
    /// Memory resource spec (e.g. '512Mi'). Defaults to '512Mi'.
    memory: Option<String>,
    /// Disk size spec (e.g. '10Gi'). Defaults to '10Gi'.
    disk_size: Option<String>,

    // -- get / inspect fields --
    /// ID of the sandbox (required for `get` and `inspect`).
    #[schemars(with = "Option<uuid::Uuid>")]
    sandbox_id: Option<SandboxId>,

    // -- inspect fields --
    /// Read-only tool to run against the sandbox (required for `inspect`): grep, glob, read, ls.
    tool: Option<InspectTool>,
    /// Tool-specific arguments for `inspect`. grep: {pattern, path?, glob?, output_mode?, ...}. glob: {pattern, path?}. read: {path, offset?, limit?}. ls: {path, ignore?}.
    #[serde(default)]
    tool_args: Option<JsonObject>,
}

// ---------------------------------------------------------------------------
// Tool
// ---------------------------------------------------------------------------

pub struct WorkspaceSandbox {
    sandboxes: Arc<Sandboxes>,
}

impl WorkspaceSandbox {
    pub fn new(sandboxes: Arc<Sandboxes>) -> Self {
        Self { sandboxes }
    }
}

static SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    let settings = schemars::gen::SchemaSettings::draft07().with(|s| {
        s.inline_subschemas = true;
        s.meta_schema = None;
    });
    let generator = settings.into_generator();
    let schema = generator.into_root_schema_for::<WorkspaceSandboxParams>();
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
impl TopLevelTool for WorkspaceSandbox {
    fn name(&self) -> &str {
        "workspace_sandbox"
    }

    fn description(&self) -> &str {
        "Sandbox management for the caller's workspace. Commands: \
         `create` (create a new sandbox — requires `name`, `mode`, optional \
         `repo_url`, `branch`, `cpu`, `memory`, `disk_size`), \
         `list` (list all sandboxes), \
         `get` (get sandbox details — requires `sandbox_id`), \
         `inspect` (run a read-only tool against a sandbox — requires \
         `sandbox_id`, `tool` (grep/glob/read/ls), and `tool_args`)."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &SCHEMA
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
        let params: WorkspaceSandboxParams = parse_params(arguments)?;

        match params.command {
            SandboxCommand::Create => {
                if !subject.can_write_workspace() {
                    return Err(ToolSetsError::Unauthorized);
                }
                let name = params.name.ok_or_else(|| {
                    ToolSetsError::MissingArgument("name is required for create".to_string())
                })?;
                let mode_enum = params.mode.ok_or_else(|| {
                    ToolSetsError::MissingArgument("mode is required for create".to_string())
                })?;

                let sandbox_mode = match mode_enum {
                    SandboxCreateMode::Repo => {
                        let repo_url = params.repo_url.ok_or_else(|| {
                            ToolSetsError::InvalidArgument(
                                "repo_url is required when mode is 'repo'".to_string(),
                            )
                        })?;
                        sandbox::SandboxMode::Repo {
                            repo_url,
                            branch: params.branch,
                        }
                    }
                    SandboxCreateMode::Scratch => sandbox::SandboxMode::Scratch,
                };

                let specs = sandbox::SandboxSpecs {
                    cpu: params.cpu.unwrap_or_else(|| "500m".to_string()),
                    memory: params.memory.unwrap_or_else(|| "512Mi".to_string()),
                    disk_size: params.disk_size.unwrap_or_else(|| "10Gi".to_string()),
                };

                let sandbox = self
                    .sandboxes
                    .create(subject, workspace_id, name, specs, sandbox_mode)
                    .await
                    .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;

                Ok(CallToolResult::success(vec![Content::text(
                    format_sandbox(&sandbox),
                )]))
            }

            SandboxCommand::List => {
                let sandboxes = self
                    .sandboxes
                    .list_for_workspace(subject, workspace_id)
                    .await
                    .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;

                Ok(CallToolResult::success(vec![Content::text(
                    format_sandboxes(&sandboxes),
                )]))
            }

            SandboxCommand::Get => {
                let sandbox_id = params.sandbox_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("sandbox_id is required for get".to_string())
                })?;

                let sandbox = self
                    .sandboxes
                    .find_by_id(subject, sandbox_id)
                    .await
                    .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;

                if sandbox.workspace_id != workspace_id {
                    return Err(ToolSetsError::Unauthorized);
                }

                Ok(CallToolResult::success(vec![Content::text(
                    format_sandbox(&sandbox),
                )]))
            }

            SandboxCommand::Inspect => {
                let sandbox_id = params.sandbox_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("sandbox_id is required for inspect".to_string())
                })?;
                let tool = params.tool.ok_or_else(|| {
                    ToolSetsError::MissingArgument("tool is required for inspect".to_string())
                })?;
                let tool_args = params.tool_args.unwrap_or_default();

                Audit::record_sandbox_id(sandbox_id);

                let sandbox = self
                    .sandboxes
                    .find_by_id(subject, sandbox_id)
                    .await
                    .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;
                if sandbox.workspace_id != workspace_id {
                    return Err(ToolSetsError::Unauthorized);
                }

                execute_inspect(subject, &self.sandboxes, sandbox_id, tool, tool_args).await
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Inspect helpers
// ---------------------------------------------------------------------------

async fn execute_inspect(
    sub: &AuthSubject,
    sandboxes: &Sandboxes,
    sandbox_id: SandboxId,
    tool: InspectTool,
    tool_args: JsonObject,
) -> Result<CallToolResult, ToolSetsError> {
    let is_ls = matches!(tool, InspectTool::Ls);

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

    let req = match tool {
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
        .instance_client_for_read(sub, sandbox_id)
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

// ---------------------------------------------------------------------------
// Formatting helpers
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
