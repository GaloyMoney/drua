//! `AdminToolSet` — admin-only tools exposed exclusively through the
//! searchable catalog (`search_tools` → `describe_tool` → `call_tool`).
//!
//! All tool implementations live here; there are no corresponding top-level
//! tool structs. Each tool is keyed by its short catalog name (without the
//! `admin_` prefix) and dispatched in [`AdminToolSet::call`].

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
// Helpers — parse params / derive schema
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
// Param structs
// ===========================================================================

// -- agent ------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
struct CreateAgentParams {
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
    #[serde(default = "default_agent_mode")]
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
struct ListAgentsParams {
    /// Workspace to list agents for.
    #[schemars(with = "uuid::Uuid")]
    workspace_id: WorkspaceId,
}

fn default_agent_mode() -> SandboxAgentMode {
    SandboxAgentMode::Read
}

// -- sandbox ----------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum SandboxCreateMode {
    /// Empty workspace.
    Scratch,
    /// Clone a repository.
    Repo,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct CreateSandboxInner {
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

impl CreateSandboxInner {
    fn into_args(self) -> Result<(String, SandboxSpecs, SandboxMode), ToolSetsError> {
        let mode = match self.mode {
            SandboxCreateMode::Repo => {
                let repo_url = self.repo_url.ok_or_else(|| {
                    ToolSetsError::InvalidArgument(
                        "repo_url is required when mode is 'repo'".to_string(),
                    )
                })?;
                SandboxMode::Repo {
                    repo_url,
                    branch: self.branch,
                }
            }
            SandboxCreateMode::Scratch => SandboxMode::Scratch,
        };

        let specs = SandboxSpecs {
            cpu: self.cpu,
            memory: self.memory,
            disk_size: self.disk_size,
        };

        Ok((self.name, specs, mode))
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
struct CreateSandboxParams {
    /// Workspace to create the sandbox in.
    #[schemars(with = "uuid::Uuid")]
    workspace_id: WorkspaceId,
    #[serde(flatten)]
    inner: CreateSandboxInner,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ListSandboxesParams {
    /// Workspace to list sandboxes for.
    #[schemars(with = "uuid::Uuid")]
    workspace_id: WorkspaceId,
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

// -- inspect ----------------------------------------------------------------

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
struct InspectSandboxParams {
    /// ID of the sandbox to inspect.
    #[schemars(with = "uuid::Uuid")]
    sandbox_id: SandboxId,
    /// Read-only tool to run against the sandbox.
    tool: InspectTool,
    /// Tool-specific arguments passed through to the sandbox. grep: {pattern, path?, glob?, output_mode?, ...}. glob: {pattern, path?}. read: {path, offset?, limit?}. ls: {path, ignore?}.
    #[serde(default)]
    arguments: JsonObject,
}

// -- audit ------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
struct QueryAuditLogParams {
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

impl QueryAuditLogParams {
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
struct CreateWorkspaceParams {
    /// Display name for the new workspace.
    name: String,
    /// Optional freeform description.
    description: Option<String>,
}

impl CreateWorkspaceParams {
    fn description(&self) -> Option<String> {
        self.description
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(String::from)
    }
}

// ===========================================================================
// Static schemas
// ===========================================================================

static CREATE_AGENT_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<CreateAgentParams>);
static ATTACH_SANDBOX_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<AttachSandboxParams>);
static DETACH_SANDBOX_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<DetachSandboxParams>);
static LIST_AGENTS_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<ListAgentsParams>);
static CREATE_SANDBOX_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<CreateSandboxParams>);
static LIST_SANDBOXES_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<ListSandboxesParams>);
static GET_SANDBOX_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<GetSandboxParams>);
static INSPECT_SANDBOX_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<InspectSandboxParams>);
static QUERY_AUDIT_LOG_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<QueryAuditLogParams>);
static CREATE_WORKSPACE_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<CreateWorkspaceParams>);
static LIST_WORKSPACES_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false,
    })
});

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
        name: "create_agent",
        description: "Create a new agent in a specified workspace (admin).",
        schema: &CREATE_AGENT_SCHEMA,
    },
    ToolDef {
        name: "attach_sandbox",
        description: "Attach a sandbox to an agent (admin).",
        schema: &ATTACH_SANDBOX_SCHEMA,
    },
    ToolDef {
        name: "detach_sandbox",
        description: "Detach a sandbox from an agent (admin).",
        schema: &DETACH_SANDBOX_SCHEMA,
    },
    ToolDef {
        name: "list_agents",
        description: "List all agents in a workspace (admin).",
        schema: &LIST_AGENTS_SCHEMA,
    },
    ToolDef {
        name: "create_sandbox",
        description: "Create a new sandbox in a specified workspace (admin).",
        schema: &CREATE_SANDBOX_SCHEMA,
    },
    ToolDef {
        name: "list_sandboxes",
        description: "List all sandboxes in a workspace (admin).",
        schema: &LIST_SANDBOXES_SCHEMA,
    },
    ToolDef {
        name: "get_sandbox",
        description: "Get sandbox details by ID (admin).",
        schema: &GET_SANDBOX_SCHEMA,
    },
    ToolDef {
        name: "inspect_sandbox",
        description: "Run a read-only tool (grep, glob, read, ls) against any sandbox (admin).",
        schema: &INSPECT_SANDBOX_SCHEMA,
    },
    ToolDef {
        name: "query_audit_log",
        description: "Query audit log entries across all workspaces.",
        schema: &QUERY_AUDIT_LOG_SCHEMA,
    },
    ToolDef {
        name: "create_workspace",
        description: "Create a new workspace (admin). Also seeds the workspace with a \
                       `WorkspaceLead` agent named 'lead'.",
        schema: &CREATE_WORKSPACE_SCHEMA,
    },
    ToolDef {
        name: "list_workspaces",
        description: "List all workspaces in the system (admin).",
        schema: &LIST_WORKSPACES_SCHEMA,
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
        "drua"
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
            "create_agent" => self.create_agent(subject, arguments).await,
            "attach_sandbox" => self.attach_sandbox(subject, arguments).await,
            "detach_sandbox" => self.detach_sandbox(subject, arguments).await,
            "list_agents" => self.list_agents(subject, arguments).await,
            "create_sandbox" => self.create_sandbox(subject, arguments).await,
            "list_sandboxes" => self.list_sandboxes(subject, arguments).await,
            "get_sandbox" => self.get_sandbox(subject, arguments).await,
            "inspect_sandbox" => self.inspect_sandbox(subject, arguments).await,
            "query_audit_log" => self.query_audit_log(arguments).await,
            "create_workspace" => self.create_workspace(subject, arguments).await,
            "list_workspaces" => self.list_workspaces(subject).await,
            _ => Err(ToolSetsError::ToolNotFound(tool_name.to_string())),
        }
    }
}

// ===========================================================================
// Tool implementations
// ===========================================================================

impl AdminToolSet {
    // -- agent --------------------------------------------------------------

    async fn create_agent(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: CreateAgentParams = parse_params(arguments)?;
        let agent = self
            .agents
            .create_agent(subject, params.workspace_id, &params.name, None)
            .await
            .map_err(|e| ToolSetsError::Agent(e.to_string()))?;
        Ok(CallToolResult::success(vec![Content::text(format_agent(
            &agent,
        ))]))
    }

    async fn attach_sandbox(
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

    async fn detach_sandbox(
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

    async fn list_agents(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: ListAgentsParams = parse_params(arguments)?;
        let agents = self
            .agents
            .list_for_workspace(subject, params.workspace_id)
            .await
            .map_err(|e| ToolSetsError::Agent(e.to_string()))?;
        Ok(CallToolResult::success(vec![Content::text(format_agents(
            &agents,
        ))]))
    }

    // -- sandbox ------------------------------------------------------------

    async fn create_sandbox(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: CreateSandboxParams = parse_params(arguments)?;
        let (name, specs, mode) = params.inner.into_args()?;
        let sandbox = self
            .sandboxes
            .create(subject, params.workspace_id, name, specs, mode)
            .await
            .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;
        Ok(CallToolResult::success(vec![Content::text(
            format_sandbox(&sandbox),
        )]))
    }

    async fn list_sandboxes(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: ListSandboxesParams = parse_params(arguments)?;
        let sandboxes = self
            .sandboxes
            .list_for_workspace(subject, params.workspace_id)
            .await
            .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;
        Ok(CallToolResult::success(vec![Content::text(
            format_sandboxes(&sandboxes),
        )]))
    }

    async fn get_sandbox(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: GetSandboxParams = parse_params(arguments)?;
        let sandbox = self
            .sandboxes
            .find_by_id(subject, params.sandbox_id)
            .await
            .map_err(|e| ToolSetsError::Sandbox(e.to_string()))?;
        Ok(CallToolResult::success(vec![Content::text(
            format_sandbox(&sandbox),
        )]))
    }

    // -- inspect ------------------------------------------------------------

    async fn inspect_sandbox(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: InspectSandboxParams = parse_params(arguments)?;
        Audit::record_sandbox_id(params.sandbox_id);
        execute_inspect(subject, &self.sandboxes, params).await
    }

    // -- audit --------------------------------------------------------------

    async fn query_audit_log(
        &self,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: QueryAuditLogParams = parse_params(arguments)?;
        let query = params.into_query();
        let entries = self.audit.find(&query).await?;
        Ok(CallToolResult::success(vec![Content::text(
            format_audit_entries(&entries),
        )]))
    }

    // -- workspace ----------------------------------------------------------

    async fn create_workspace(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: CreateWorkspaceParams = parse_params(arguments)?;
        let workspace = self
            .workspaces
            .create(subject, &params.name, params.description())
            .await
            .map_err(|e| ToolSetsError::Workspace(e.to_string()))?;
        Ok(CallToolResult::success(vec![Content::text(
            format_workspace_created(&workspace),
        )]))
    }

    async fn list_workspaces(
        &self,
        subject: &AuthSubject,
    ) -> Result<CallToolResult, ToolSetsError> {
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

// ===========================================================================
// Inspect helpers
// ===========================================================================

async fn execute_inspect(
    sub: &AuthSubject,
    sandboxes: &Sandboxes,
    params: InspectSandboxParams,
) -> Result<CallToolResult, ToolSetsError> {
    let is_ls = matches!(params.tool, InspectTool::Ls);
    let tool_args = params.arguments;

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
        .instance_client_for(sub, params.sandbox_id)
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
