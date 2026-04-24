//! `AdminToolSet` — admin-only tools exposed exclusively through the
//! searchable catalog (`search_tools` → `describe_tool` → `call_tool`).
//!
//! Consolidated into 4 tools with command discriminators:
//! `agent`, `sandbox`, `log`, `workspace`.
//! Prefixed as `drua_admin_agent`, `drua_admin_sandbox`, etc.

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use sandbox::instance_client::ExecuteRequest;
use serde::Deserialize;

use crate::agent::{Agent, AgentRole, Agents};
use crate::audit::{Audit, AuditEntry, AuditLogQuery};
use crate::auth::AuthSubject;
use crate::primitives::{AgentId, SandboxId, UserId, WorkspaceId};
use crate::sandbox::{Sandbox, SandboxAgentMode, SandboxMode, SandboxSpecs, Sandboxes};
use crate::workspace::{Workspace, Workspaces};

use super::super::error::ToolSetsError;
use super::super::traits::{SearchableToolSet, ToolSetEntry};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_params<T: serde::de::DeserializeOwned>(
    arguments: Option<JsonObject>,
) -> Result<T, ToolSetsError> {
    let value = serde_json::Value::Object(arguments.unwrap_or_default());
    serde_json::from_value(value).map_err(|e| ToolSetsError::InvalidArgument(e.to_string()))
}

fn schema_for<T: schemars::JsonSchema>() -> serde_json::Value {
    let settings = schemars::gen::SchemaSettings::draft07().with(|s| {
        s.inline_subschemas = true;
        s.meta_schema = None;
    });
    let generator = settings.into_generator();
    let schema = generator.into_root_schema_for::<T>();
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
}

/// Deserialize an `i64` from either a JSON number or a string like `"20"`.
fn deserialize_liberal_i64<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<i64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrInt {
        Int(i64),
        Str(String),
    }
    match StringOrInt::deserialize(deserializer)? {
        StringOrInt::Int(v) => Ok(v),
        StringOrInt::Str(s) => s.parse().map_err(serde::de::Error::custom),
    }
}

// ===========================================================================
// Param structs — consolidated with command discriminators
// ===========================================================================

// -- agent ------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum AgentCommand {
    /// Create a new agent in a workspace.
    Create,
    /// List all agents in a workspace.
    List,
    /// Attach a sandbox to an agent.
    AttachSandbox,
    /// Detach a sandbox from an agent.
    DetachSandbox,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct AgentParams {
    /// Which agent operation to perform.
    command: AgentCommand,
    /// Workspace ID (required for `create` and `list`).
    #[schemars(with = "Option<uuid::Uuid>")]
    workspace_id: Option<WorkspaceId>,
    /// Display name for the new agent (required for `create`).
    name: Option<String>,
    /// ID of the agent (required for `attach_sandbox` and `detach_sandbox`).
    #[schemars(with = "Option<uuid::Uuid>")]
    agent_id: Option<AgentId>,
    /// ID of the sandbox (required for `attach_sandbox` and `detach_sandbox`).
    #[schemars(with = "Option<uuid::Uuid>")]
    sandbox_id: Option<SandboxId>,
    /// Attach mode — 'read' or 'use'. Defaults to 'read'. Only for `attach_sandbox`.
    mode: Option<SandboxAgentMode>,
}

// -- sandbox ----------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum SandboxCommand {
    /// Create a new sandbox in a workspace.
    Create,
    /// List all sandboxes in a workspace.
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
struct SandboxParams {
    /// Which sandbox operation to perform.
    command: SandboxCommand,

    // -- create / list fields --
    /// Workspace ID (required for `create` and `list`).
    #[schemars(with = "Option<uuid::Uuid>")]
    workspace_id: Option<WorkspaceId>,
    /// Display name for the new sandbox (required for `create`).
    name: Option<String>,
    /// Sandbox mode: 'scratch' or 'repo' (required for `create`).
    mode: Option<SandboxCreateMode>,
    /// Repository URL to clone (required when mode is 'repo').
    repo_url: Option<String>,
    /// Git branch to check out after cloning (optional).
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

// -- log --------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
struct LogParams {
    /// Substring filter on entrypoint (e.g. 'api', 'mcp', 'graphql').
    entrypoint: Option<String>,
    /// Substring filter on action (e.g. 'workspace.create', 'catalog:').
    action: Option<String>,
    /// Substring filter on outcome (e.g. 'success', 'error').
    outcome: Option<String>,
    /// When true, return only entries that resulted in an error.
    errors_only: Option<bool>,
    /// Filter by acting user ID.
    #[schemars(with = "Option<uuid::Uuid>")]
    user_id: Option<UserId>,
    /// Filter by acting agent ID.
    #[schemars(with = "Option<uuid::Uuid>")]
    agent_id: Option<AgentId>,
    /// Filter by sandbox ID.
    #[schemars(with = "Option<uuid::Uuid>")]
    sandbox_id: Option<SandboxId>,
    /// Max entries to return (1-100, default 20).
    #[serde(
        default = "default_audit_limit",
        deserialize_with = "deserialize_liberal_i64"
    )]
    limit: i64,
}

fn default_audit_limit() -> i64 {
    20
}

impl LogParams {
    fn into_query(self) -> AuditLogQuery {
        let entrypoint = self
            .entrypoint
            .filter(|s| !s.is_empty())
            .map(|s| format!("%{s}%"));
        let action = self
            .action
            .filter(|s| !s.is_empty())
            .map(|s| format!("%{s}%"));
        let outcome = self
            .outcome
            .filter(|s| !s.is_empty())
            .map(|s| format!("%{s}%"));
        let error = self.errors_only.and_then(|b| b.then_some(true));

        AuditLogQuery {
            limit: self.limit.clamp(1, 100),
            entrypoint,
            action,
            outcome,
            acting_user_id: self.user_id,
            acting_agent_id: self.agent_id,
            sandbox_id: self.sandbox_id,
            error,
            ..Default::default()
        }
    }
}

// -- workspace --------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum WorkspaceCommand {
    /// Create a new workspace (also seeds a WorkspaceLead agent named 'lead').
    Create,
    /// List all workspaces in the system.
    List,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct WorkspaceParams {
    /// Which workspace operation to perform.
    command: WorkspaceCommand,
    /// Display name for the new workspace (required for `create`).
    name: Option<String>,
    /// Optional freeform description (for `create`).
    description: Option<String>,
}

// ===========================================================================
// Static schemas
// ===========================================================================

static AGENT_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<AgentParams>);
static SANDBOX_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<SandboxParams>);
static LOG_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<LogParams>);
static WORKSPACE_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<WorkspaceParams>);

// ===========================================================================
// Tool descriptors
// ===========================================================================

struct ToolDef {
    name: &'static str,
    description: &'static str,
    schema: &'static LazyLock<serde_json::Value>,
}

static TOOLS: &[ToolDef] = &[
    ToolDef {
        name: "agent",
        description: "Manage agents. Commands: `create` (requires `workspace_id`, `name`), \
                       `list` (requires `workspace_id`), \
                       `attach_sandbox` (requires `agent_id`, `sandbox_id`, optional `mode`), \
                       `detach_sandbox` (requires `agent_id`, `sandbox_id`).",
        schema: &AGENT_SCHEMA,
    },
    ToolDef {
        name: "sandbox",
        description: "Manage sandboxes. Commands: `create` (requires `workspace_id`, `name`, \
                       `mode`, optional `repo_url`, `branch`, `cpu`, `memory`, `disk_size`), \
                       `list` (requires `workspace_id`), \
                       `get` (requires `sandbox_id`), \
                       `inspect` (requires `sandbox_id`, `tool` (grep/glob/read/ls), `tool_args`).",
        schema: &SANDBOX_SCHEMA,
    },
    ToolDef {
        name: "log",
        description: "Query audit log entries across all workspaces.",
        schema: &LOG_SCHEMA,
    },
    ToolDef {
        name: "workspace",
        description: "Manage workspaces. Commands: `create` (requires `name`, optional \
                       `description`; also seeds a WorkspaceLead agent), `list`.",
        schema: &WORKSPACE_SCHEMA,
    },
];

// ===========================================================================
// AdminToolSet
// ===========================================================================

pub struct AdminToolSet {
    entries: Vec<ToolSetEntry>,
    agents: Arc<Agents>,
    sandboxes: Arc<Sandboxes>,
    audit: Arc<Audit>,
    workspaces: Arc<Workspaces>,
}

impl AdminToolSet {
    pub fn new(
        agents: Arc<Agents>,
        sandboxes: Arc<Sandboxes>,
        audit: Arc<Audit>,
        workspaces: Arc<Workspaces>,
    ) -> Self {
        let entries = TOOLS
            .iter()
            .map(|t| ToolSetEntry {
                name: t.name.to_string(),
                description: rmcp::model::Tool::new(
                    t.name.to_string(),
                    t.description.to_string(),
                    serde_json::from_value::<JsonObject>((*t.schema).clone()).unwrap_or_default(),
                ),
                default_output_filter: None,
            })
            .collect();

        Self {
            entries,
            agents,
            sandboxes,
            audit,
            workspaces,
        }
    }
}

#[async_trait::async_trait]
impl SearchableToolSet for AdminToolSet {
    fn name(&self) -> &str {
        "drua_admin"
    }

    fn category(&self) -> &str {
        "admin"
    }

    fn category_description(&self) -> &str {
        "Workspace, agent, and sandbox management (admin)"
    }

    fn tools(&self) -> &[ToolSetEntry] {
        &self.entries
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.is_admin()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        tool_name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        match tool_name {
            "agent" => self.agent(subject, arguments).await,
            "sandbox" => self.sandbox(subject, arguments).await,
            "log" => self.log(arguments).await,
            "workspace" => self.workspace(subject, arguments).await,
            _ => Err(ToolSetsError::ToolNotFound(tool_name.to_string())),
        }
    }
}

// ===========================================================================
// Tool implementations
// ===========================================================================

impl AdminToolSet {
    // -- agent --------------------------------------------------------------

    async fn agent(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: AgentParams = parse_params(arguments)?;

        match params.command {
            AgentCommand::Create => {
                let workspace_id = params.workspace_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument(
                        "workspace_id is required for create".to_string(),
                    )
                })?;
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

            AgentCommand::List => {
                let workspace_id = params.workspace_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("workspace_id is required for list".to_string())
                })?;
                let agents = self
                    .agents
                    .list_for_workspace(subject, workspace_id)
                    .await
                    .map_err(|e| ToolSetsError::Agent(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(format_agents(
                    &agents,
                ))]))
            }

            AgentCommand::AttachSandbox => {
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

            AgentCommand::DetachSandbox => {
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
        }
    }

    // -- sandbox ------------------------------------------------------------

    async fn sandbox(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: SandboxParams = parse_params(arguments)?;

        match params.command {
            SandboxCommand::Create => {
                let workspace_id = params.workspace_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument(
                        "workspace_id is required for create".to_string(),
                    )
                })?;
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
                        SandboxMode::Repo {
                            repo_url,
                            branch: params.branch,
                        }
                    }
                    SandboxCreateMode::Scratch => SandboxMode::Scratch,
                };

                let specs = SandboxSpecs {
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
                let workspace_id = params.workspace_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("workspace_id is required for list".to_string())
                })?;
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
                execute_inspect(subject, &self.sandboxes, sandbox_id, tool, tool_args).await
            }
        }
    }

    // -- log ----------------------------------------------------------------

    async fn log(&self, arguments: Option<JsonObject>) -> Result<CallToolResult, ToolSetsError> {
        let params: LogParams = parse_params(arguments)?;
        let query = params.into_query();
        let entries = self.audit.find(&query).await?;
        Ok(CallToolResult::success(vec![Content::text(
            format_audit_entries(&entries),
        )]))
    }

    // -- workspace ----------------------------------------------------------

    async fn workspace(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: WorkspaceParams = parse_params(arguments)?;

        match params.command {
            WorkspaceCommand::Create => {
                let name = params.name.ok_or_else(|| {
                    ToolSetsError::MissingArgument("name is required for create".to_string())
                })?;
                let description = params
                    .description
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map(String::from);
                let workspace = self
                    .workspaces
                    .create(subject, &name, description)
                    .await
                    .map_err(|e| ToolSetsError::Workspace(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(
                    format_workspace_created(&workspace),
                )]))
            }

            WorkspaceCommand::List => {
                let all = self
                    .workspaces
                    .list_all(subject)
                    .await
                    .map_err(|e| ToolSetsError::Workspace(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(
                    format_workspaces(&all),
                )]))
            }
        }
    }
}

// ===========================================================================
// Inspect helpers
// ===========================================================================

async fn execute_inspect(
    sub: &AuthSubject,
    sandboxes: &Sandboxes,
    sandbox_id: SandboxId,
    tool: InspectTool,
    tool_args: JsonObject,
) -> Result<CallToolResult, ToolSetsError> {
    let is_ls = matches!(tool, InspectTool::Ls);

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
        .instance_client_for(sub, sandbox_id)
        .await
        .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;

    match client.execute(&req).await {
        Ok(resp) => {
            let mut output = resp.output;
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
        let start = offset.unwrap_or(0) + 1;
        let end = match limit {
            Some(l) => start + l - 1,
            None => -1,
        };
        input["view_range"] = serde_json::json!([start, end]);
    }

    Ok(ExecuteRequest {
        tool: "str_replace_based_edit_tool".to_string(),
        input,
    })
}

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

// ===========================================================================
// Formatting helpers
// ===========================================================================

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
    }
}

// -- agent ------------------------------------------------------------------

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
            None => "\u{2014}".to_string(),
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

// -- sandbox ----------------------------------------------------------------

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

// -- audit ------------------------------------------------------------------

fn format_audit_entries(entries: &[AuditEntry]) -> String {
    if entries.is_empty() {
        return "No audit entries found.".to_string();
    }

    let mut lines = Vec::with_capacity(entries.len() + 2);
    lines.push(format!(
        "{:<6} {:<20} {:<10} {:<26} {:<26} {:<8} {:>8}",
        "ID", "TIME", "TYPE", "ENTRYPOINT", "ACTION", "OUTCOME", "MS"
    ));
    lines.push("-".repeat(108));

    for e in entries {
        let ts = e.recorded_at.format("%Y-%m-%d %H:%M:%S");
        let dur = e.duration_ms.map(|ms| format!("{ms}")).unwrap_or_default();
        let ep = e.entrypoint.as_deref().unwrap_or("");
        lines.push(format!(
            "{:<6} {:<20} {:<10} {:<26} {:<26} {:<8} {:>8}",
            e.id,
            ts,
            truncate(&e.interaction_type, 10),
            truncate(ep, 26),
            truncate(&e.action, 26),
            truncate(&e.outcome, 8),
            dur,
        ));
    }

    lines.join("\n")
}

// -- workspace --------------------------------------------------------------

fn format_workspace_created(w: &Workspace) -> String {
    let description = w.description.as_deref().unwrap_or("\u{2014}");
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
        let description = w.description.as_deref().unwrap_or("\u{2014}");
        lines.push(format!("{:<38} {:<30} {}", w.id, w.name, description));
    }
    lines.join("\n")
}
