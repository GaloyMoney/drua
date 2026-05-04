//! `AdminToolSet` — admin-only tools exposed exclusively through the
//! searchable catalog (`search_tools` → `describe_tool` → `call_tool`).
//!
//! Consolidated into 5 tools with command discriminators:
//! `agent`, `sandbox`, `log`, `project`, `spaces`.
//! Prefixed as `drua_admin_agent`, `drua_admin_sandbox`, etc.

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use sandbox::instance_client::ExecuteRequest;
use serde::Deserialize;

use drua_library::{Space, SpaceError};

use crate::agent::{Agent, AgentRole, Agents};
use crate::audit::{Audit, AuditEntry, AuditLogQuery};
use crate::auth::{AuthResource, AuthSubject, AuthVerb};
use crate::library::AuthedSpaces;
use crate::primitives::{AgentId, ProjectId, SandboxId, UserId};
use crate::project::{Project, Projects};
use crate::sandbox::{Sandbox, SandboxAgentMode, SandboxMode, SandboxSpecs, Sandboxes};
use crate::space_fs::SpaceFs;

use super::super::error::ToolSetsError;
use super::super::inspect::{dispatch_inspect, parse_view_range, require_space_op, InspectTool};
use super::super::traits::{SearchableToolSet, ToolSetEntry};

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
        // `definitions` retained for $ref resolution by the compose TS generator.
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

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum AgentCommand {
    Create,
    List,
    AttachSandbox,
    DetachSandbox,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct AgentParams {
    /// Which agent operation to perform.
    command: AgentCommand,
    /// Project ID (required for `create` and `list`).
    #[schemars(with = "Option<uuid::Uuid>")]
    project_id: Option<ProjectId>,
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

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum SandboxCommand {
    Create,
    List,
    Get,
    Inspect,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum SandboxCreateMode {
    Scratch,
    Repo,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct SandboxParams {
    command: SandboxCommand,

    #[schemars(with = "Option<uuid::Uuid>")]
    project_id: Option<ProjectId>,
    name: Option<String>,
    mode: Option<SandboxCreateMode>,
    repo_url: Option<String>,
    branch: Option<String>,
    cpu: Option<String>,
    memory: Option<String>,
    disk_size: Option<String>,

    #[schemars(with = "Option<uuid::Uuid>")]
    sandbox_id: Option<SandboxId>,

    tool: Option<InspectTool>,
    #[serde(default)]
    tool_args: Option<JsonObject>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct LogParams {
    entrypoint: Option<String>,
    action: Option<String>,
    outcome: Option<String>,
    errors_only: Option<bool>,
    #[schemars(with = "Option<uuid::Uuid>")]
    user_id: Option<UserId>,
    #[schemars(with = "Option<uuid::Uuid>")]
    agent_id: Option<AgentId>,
    #[schemars(with = "Option<uuid::Uuid>")]
    sandbox_id: Option<SandboxId>,
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

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ProjectCommand {
    /// Create a new project (also seeds a ProjectLead agent named 'lead').
    Create,
    List,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ProjectParams {
    command: ProjectCommand,
    name: Option<String>,
    description: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum SpacesCommand {
    Create,
    List,
    Get,
    /// Attach an existing space to any project. Admin variant —
    /// project_id is explicit (the tool isn't bound to a project).
    Mount,
    /// Detach a space from any project. Project-scoped admins use the
    /// top-level `spaces` tool instead; this admin variant lets you
    /// reach into any project.
    Unmount,
    /// Read-only file ops on a space. Mirrors sandbox admin's
    /// `inspect`: pass `tool` (read|ls|grep|glob) and `tool_args`.
    Inspect,
    /// Blind overwrite of `space:<slug>/<path>` with `content`.
    Write,
    /// Delete `space:<slug>/<path>`.
    Delete,
    /// Rename / move `space:<slug>/<from>` → `space:<slug>/<to>`.
    Move,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct SpacesParams {
    command: SpacesCommand,
    /// Slug for `create`, `get`, `mount`, `unmount`, `inspect`,
    /// `write`, `delete`, `move`. Must match `[a-z0-9-]+` with no
    /// leading / trailing / double hyphens.
    slug: Option<String>,
    /// Optional human-readable summary, used by `create`.
    description: Option<String>,
    /// Required for `mount` and `unmount`. The project to attach the
    /// space to (or detach from).
    #[schemars(with = "Option<uuid::Uuid>")]
    project_id: Option<ProjectId>,

    /// Required for `inspect`. Selects the read-only sub-op.
    tool: Option<InspectTool>,
    /// Op-specific arguments for `inspect`. Shape mirrors the
    /// equivalent top-level tool (`Read` / `LS` / `Grep` / `Glob`).
    #[serde(default)]
    tool_args: Option<JsonObject>,

    /// Path required for `write` and `delete`; relative to
    /// `spaces/<slug>/`.
    path: Option<String>,
    /// Required for `write`.
    content: Option<String>,
    /// Required for `move` (source path within the space).
    from: Option<String>,
    /// Required for `move` (destination path within the space).
    to: Option<String>,
}

static AGENT_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<AgentParams>);
static SANDBOX_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<SandboxParams>);
static LOG_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<LogParams>);
static PROJECT_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<ProjectParams>);
static SPACES_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<SpacesParams>);

struct ToolDef {
    name: &'static str,
    description: &'static str,
    schema: &'static LazyLock<serde_json::Value>,
}

static TOOLS: &[ToolDef] = &[
    ToolDef {
        name: "agent",
        description: "Manage agents. Commands: `create` (requires `project_id`, `name`), \
                       `list` (requires `project_id`), \
                       `attach_sandbox` (requires `agent_id`, `sandbox_id`, optional `mode`), \
                       `detach_sandbox` (requires `agent_id`, `sandbox_id`).",
        schema: &AGENT_SCHEMA,
    },
    ToolDef {
        name: "sandbox",
        description: "Manage sandboxes. Commands: `create` (requires `project_id`, `name`, \
                       `mode`, optional `repo_url`, `branch`, `cpu`, `memory`, `disk_size`), \
                       `list` (requires `project_id`), \
                       `get` (requires `sandbox_id`), \
                       `inspect` (requires `sandbox_id`, `tool` (grep/glob/read/ls), `tool_args`).",
        schema: &SANDBOX_SCHEMA,
    },
    ToolDef {
        name: "log",
        description: "Query audit log entries across all projects.",
        schema: &LOG_SCHEMA,
    },
    ToolDef {
        name: "project",
        description: "Manage projects. Commands: `create` (requires `name`, optional \
                       `description`; also seeds a ProjectLead agent), `list`.",
        schema: &PROJECT_SCHEMA,
    },
    ToolDef {
        name: "spaces",
        description: "Manage library spaces — bounded collaborative folders under \
                       `spaces/<slug>/` in the knowledge-base repo. Commands: \
                       `create` (requires `slug`, optional `description`; the space \
                       is created unmounted — pair with `mount` to attach), \
                       `list` (no args; every space in the library), \
                       `get` (requires `slug`), \
                       `mount` (requires `slug` and `project_id`; idempotent), \
                       `unmount` (requires `slug` and `project_id`; idempotent), \
                       `inspect` (read-only file ops; requires `slug`, `tool` \
                       (read|ls|grep|glob), `tool_args`), \
                       `write` (requires `slug`, `path`, `content`), \
                       `delete` (requires `slug`, `path`), \
                       `move` (requires `slug`, `from`, `to`).",
        schema: &SPACES_SCHEMA,
    },
];

pub struct AdminToolSet {
    entries: Vec<ToolSetEntry>,
    agents: Arc<Agents>,
    sandboxes: Arc<Sandboxes>,
    audit: Arc<Audit>,
    projects: Arc<Projects>,
    spaces: Arc<AuthedSpaces>,
    space_fs: Arc<SpaceFs>,
}

impl AdminToolSet {
    pub fn new(
        agents: Arc<Agents>,
        sandboxes: Arc<Sandboxes>,
        audit: Arc<Audit>,
        projects: Arc<Projects>,
        spaces: Arc<AuthedSpaces>,
        space_fs: Arc<SpaceFs>,
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
            projects,
            spaces,
            space_fs,
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
        "Project, agent, and sandbox management (admin)"
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
            "project" => self.project(subject, arguments).await,
            "spaces" => self.spaces(subject, arguments).await,
            _ => Err(ToolSetsError::ToolNotFound(tool_name.to_string())),
        }
    }
}

impl AdminToolSet {
    async fn agent(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: AgentParams = parse_params(arguments)?;

        match params.command {
            AgentCommand::Create => {
                let project_id = params.project_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("project_id is required for create".to_string())
                })?;
                let name = params.name.ok_or_else(|| {
                    ToolSetsError::MissingArgument("name is required for create".to_string())
                })?;
                let agent = self
                    .agents
                    .create_agent(subject, project_id, &name, None)
                    .await
                    .map_err(|e| ToolSetsError::Agent(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(format_agent(
                    &agent,
                ))]))
            }

            AgentCommand::List => {
                let project_id = params.project_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("project_id is required for list".to_string())
                })?;
                let agents = self
                    .agents
                    .list_for_project(subject, project_id)
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

    async fn sandbox(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: SandboxParams = parse_params(arguments)?;

        match params.command {
            SandboxCommand::Create => {
                let project_id = params.project_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("project_id is required for create".to_string())
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
                    .create(subject, project_id, name, specs, sandbox_mode)
                    .await?;
                Ok(CallToolResult::success(vec![Content::text(
                    format_sandbox(&sandbox),
                )]))
            }

            SandboxCommand::List => {
                let project_id = params.project_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("project_id is required for list".to_string())
                })?;
                let sandboxes = self.sandboxes.list_for_project(subject, project_id).await?;
                Ok(CallToolResult::success(vec![Content::text(
                    format_sandboxes(&sandboxes),
                )]))
            }

            SandboxCommand::Get => {
                let sandbox_id = params.sandbox_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("sandbox_id is required for get".to_string())
                })?;
                let sandbox = self.sandboxes.find_by_id(subject, sandbox_id).await?;
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

    async fn log(&self, arguments: Option<JsonObject>) -> Result<CallToolResult, ToolSetsError> {
        let params: LogParams = parse_params(arguments)?;
        let query = params.into_query();
        let entries = self.audit.find(&query).await?;
        Ok(CallToolResult::success(vec![Content::text(
            format_audit_entries(&entries),
        )]))
    }

    async fn project(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: ProjectParams = parse_params(arguments)?;

        match params.command {
            ProjectCommand::Create => {
                let name = params.name.ok_or_else(|| {
                    ToolSetsError::MissingArgument("name is required for create".to_string())
                })?;
                let description = params
                    .description
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map(String::from);
                let project = self.projects.create(subject, &name, description).await?;
                Ok(CallToolResult::success(vec![Content::text(
                    format_project_created(&project),
                )]))
            }

            ProjectCommand::List => {
                let all = self.projects.list_all(subject).await?;
                Ok(CallToolResult::success(vec![Content::text(
                    format_projects(&all),
                )]))
            }
        }
    }

    async fn spaces(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: SpacesParams = parse_params(arguments)?;

        match params.command {
            SpacesCommand::Create => {
                let slug = params.slug.ok_or_else(|| {
                    ToolSetsError::MissingArgument("slug is required for create".to_string())
                })?;
                let description = params.description.filter(|s| !s.is_empty());
                Audit::record_action("spaces.create");
                let space = self.spaces.create(subject, slug, description).await?;
                Ok(CallToolResult::success(vec![Content::text(format_space(
                    &space, true,
                ))]))
            }

            SpacesCommand::List => {
                Audit::record_action("spaces.list");
                let all = self.spaces.list_all(subject).await?;
                Ok(CallToolResult::success(vec![Content::text(format_spaces(
                    &all,
                ))]))
            }

            SpacesCommand::Get => {
                let slug = params.slug.ok_or_else(|| {
                    ToolSetsError::MissingArgument("slug is required for get".to_string())
                })?;
                Audit::record_action("spaces.get");
                subject
                    .can(AuthVerb::Read, AuthResource::Space(None))
                    .map_err(|e| ToolSetsError::Library(e.into()))?;
                let space =
                    self.spaces.find_by_slug(&slug).await?.ok_or_else(|| {
                        ToolSetsError::Library(SpaceError::NotFound { slug }.into())
                    })?;
                Ok(CallToolResult::success(vec![Content::text(format_space(
                    &space, false,
                ))]))
            }

            SpacesCommand::Mount => {
                let slug = params.slug.ok_or_else(|| {
                    ToolSetsError::MissingArgument("slug is required for mount".to_string())
                })?;
                let project_id = params.project_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("project_id is required for mount".to_string())
                })?;
                Audit::record_action("spaces.mount");
                let space = self
                    .projects
                    .mount_space(subject, project_id, &slug)
                    .await?;
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "Space mounted onto project {project_id}.\n  slug: {}\n  id: {}",
                    space.slug, space.id,
                ))]))
            }

            SpacesCommand::Unmount => {
                let slug = params.slug.ok_or_else(|| {
                    ToolSetsError::MissingArgument("slug is required for unmount".to_string())
                })?;
                let project_id = params.project_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("project_id is required for unmount".to_string())
                })?;
                Audit::record_action("spaces.unmount");
                let space =
                    self.spaces.find_by_slug(&slug).await?.ok_or_else(|| {
                        ToolSetsError::Library(SpaceError::NotFound { slug }.into())
                    })?;
                self.projects
                    .unmount_space(subject, project_id, space.id)
                    .await?;
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "Space unmounted from project {project_id}.\n  slug: {}\n  id: {}",
                    space.slug, space.id,
                ))]))
            }

            SpacesCommand::Inspect => {
                let slug = params.slug.ok_or_else(|| {
                    ToolSetsError::MissingArgument("slug is required for inspect".to_string())
                })?;
                let tool = params.tool.ok_or_else(|| {
                    ToolSetsError::MissingArgument("tool is required for inspect".to_string())
                })?;
                let tool_args = params.tool_args.unwrap_or_default();
                Audit::record_action("spaces.inspect");
                dispatch_inspect(&self.space_fs, subject, &slug, tool, tool_args).await
            }

            SpacesCommand::Write => {
                let slug = params.slug.ok_or_else(|| {
                    ToolSetsError::MissingArgument("slug is required for write".to_string())
                })?;
                let path = params.path.ok_or_else(|| {
                    ToolSetsError::MissingArgument("path is required for write".to_string())
                })?;
                let content = params.content.ok_or_else(|| {
                    ToolSetsError::MissingArgument("content is required for write".to_string())
                })?;
                Audit::record_action("spaces.write");
                let space_path = format!("space:{slug}/{path}");
                let result = self
                    .space_fs
                    .write_file(subject, &space_path, content)
                    .await?;
                require_space_op(result, "write")?;
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "Wrote {space_path}"
                ))]))
            }

            SpacesCommand::Delete => {
                let slug = params.slug.ok_or_else(|| {
                    ToolSetsError::MissingArgument("slug is required for delete".to_string())
                })?;
                let path = params.path.ok_or_else(|| {
                    ToolSetsError::MissingArgument("path is required for delete".to_string())
                })?;
                Audit::record_action("spaces.delete");
                let space_path = format!("space:{slug}/{path}");
                let result = self.space_fs.delete_file(subject, &space_path).await?;
                require_space_op(result, "delete")?;
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "Deleted {space_path}"
                ))]))
            }

            SpacesCommand::Move => {
                let slug = params.slug.ok_or_else(|| {
                    ToolSetsError::MissingArgument("slug is required for move".to_string())
                })?;
                let from = params.from.ok_or_else(|| {
                    ToolSetsError::MissingArgument("from is required for move".to_string())
                })?;
                let to = params.to.ok_or_else(|| {
                    ToolSetsError::MissingArgument("to is required for move".to_string())
                })?;
                Audit::record_action("spaces.move");
                let from_path = format!("space:{slug}/{from}");
                let to_path = format!("space:{slug}/{to}");
                let result = self
                    .space_fs
                    .move_file(subject, &from_path, &to_path)
                    .await?;
                require_space_op(result, "move")?;
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "Moved {from_path} -> {to_path}"
                ))]))
            }
        }
    }
}

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

    let client = sandboxes.instance_client_for(sub, sandbox_id).await?;

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

    if let Some((start, end)) = parse_view_range(args) {
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

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
    }
}

fn format_agent(a: &Agent) -> String {
    let role = match a.agent_role {
        AgentRole::ProjectLead => "project_lead",
        AgentRole::Agent => "agent",
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
            None => "\u{2014}".to_string(),
        };
        let role = match a.agent_role {
            AgentRole::ProjectLead => "project_lead",
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
        "Sandbox:\n  id: {}\n  name: {}\n  project: {}\n  state: {}\n  mode: {:?}\n  specs: cpu={}, mem={}, disk={}{}\n  attached_agents:\n{}",
        s.id, s.name, s.project_id, s.state, s.mode,
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

fn format_project_created(w: &Project) -> String {
    let description = w.description.as_deref().unwrap_or("\u{2014}");
    format!(
        "Project created.\n  id: {}\n  name: {}\n  description: {}",
        w.id, w.name, description
    )
}

fn format_projects(project: &[Project]) -> String {
    if project.is_empty() {
        return "No projects found.".to_string();
    }

    let mut lines = Vec::with_capacity(project.len() + 2);
    lines.push(format!("{:<38} {:<30} {}", "ID", "NAME", "DESCRIPTION"));
    lines.push("-".repeat(100));
    for w in project {
        let description = w.description.as_deref().unwrap_or("\u{2014}");
        lines.push(format!("{:<38} {:<30} {}", w.id, w.name, description));
    }
    lines.join("\n")
}

fn format_space(s: &Space, created: bool) -> String {
    let description = s.description.as_deref().unwrap_or("\u{2014}");
    let header = if created { "Space created." } else { "Space:" };
    format!(
        "{header}\n  id: {}\n  slug: {}\n  description: {}",
        s.id, s.slug, description,
    )
}

fn format_spaces(spaces: &[Space]) -> String {
    if spaces.is_empty() {
        return "No spaces found.".to_string();
    }

    let mut lines = Vec::with_capacity(spaces.len() + 2);
    lines.push(format!("{:<38} {:<24} {}", "ID", "SLUG", "DESCRIPTION"));
    lines.push("-".repeat(100));

    for s in spaces {
        let description = s.description.as_deref().unwrap_or("\u{2014}");
        lines.push(format!(
            "{:<38} {:<24} {}",
            s.id,
            truncate(&s.slug, 24),
            description,
        ));
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(json: serde_json::Value) -> Option<JsonObject> {
        match json {
            serde_json::Value::Object(obj) => Some(obj),
            _ => panic!("expected object"),
        }
    }

    #[test]
    fn spaces_create_parses_full_args() {
        let p: SpacesParams = parse_params(args(serde_json::json!({
            "command": "create",
            "slug": "oncall",
            "description": "On-call rotation",
        })))
        .expect("parse");
        assert!(matches!(p.command, SpacesCommand::Create));
        assert_eq!(p.slug.as_deref(), Some("oncall"));
        assert_eq!(p.description.as_deref(), Some("On-call rotation"));
    }

    #[test]
    fn spaces_create_parses_without_description() {
        let p: SpacesParams = parse_params(args(serde_json::json!({
            "command": "create",
            "slug": "incidents",
        })))
        .expect("parse");
        assert!(matches!(p.command, SpacesCommand::Create));
        assert_eq!(p.slug.as_deref(), Some("incidents"));
        assert!(p.description.is_none());
    }

    #[test]
    fn spaces_list_parses_no_args() {
        let p: SpacesParams = parse_params(args(serde_json::json!({
            "command": "list",
        })))
        .expect("parse");
        assert!(matches!(p.command, SpacesCommand::List));
        assert!(p.slug.is_none());
        assert!(p.description.is_none());
    }

    #[test]
    fn spaces_get_parses_slug() {
        let p: SpacesParams = parse_params(args(serde_json::json!({
            "command": "get",
            "slug": "oncall",
        })))
        .expect("parse");
        assert!(matches!(p.command, SpacesCommand::Get));
        assert_eq!(p.slug.as_deref(), Some("oncall"));
    }

    #[test]
    fn spaces_rejects_unknown_command() {
        let res: Result<SpacesParams, _> = parse_params(args(serde_json::json!({
            "command": "destroy",
        })));
        assert!(res.is_err());
    }

    #[test]
    fn spaces_mount_parses_slug_and_project_id() {
        let project_id = uuid::Uuid::new_v4();
        let p: SpacesParams = parse_params(args(serde_json::json!({
            "command": "mount",
            "slug": "oncall",
            "project_id": project_id,
        })))
        .expect("parse");
        assert!(matches!(p.command, SpacesCommand::Mount));
        assert_eq!(p.slug.as_deref(), Some("oncall"));
        assert_eq!(p.project_id.map(uuid::Uuid::from), Some(project_id));
    }

    #[test]
    fn spaces_unmount_parses_slug_and_project_id() {
        let project_id = uuid::Uuid::new_v4();
        let p: SpacesParams = parse_params(args(serde_json::json!({
            "command": "unmount",
            "slug": "oncall",
            "project_id": project_id,
        })))
        .expect("parse");
        assert!(matches!(p.command, SpacesCommand::Unmount));
        assert_eq!(p.slug.as_deref(), Some("oncall"));
    }

    #[test]
    fn spaces_schema_includes_command_enum() {
        let schema = &*SPACES_SCHEMA;
        let s = serde_json::to_string(schema).unwrap();
        assert!(s.contains("\"create\""));
        assert!(s.contains("\"list\""));
        assert!(s.contains("\"get\""));
        assert!(s.contains("\"mount\""));
        assert!(s.contains("\"unmount\""));
        assert!(s.contains("\"inspect\""));
        assert!(s.contains("\"write\""));
        assert!(s.contains("\"delete\""));
        assert!(s.contains("\"move\""));
    }

    #[test]
    fn spaces_inspect_parses_full_args() {
        let p: SpacesParams = parse_params(args(serde_json::json!({
            "command": "inspect",
            "slug": "oncall",
            "tool": "read",
            "tool_args": { "path": "runbooks/foo.md" },
        })))
        .expect("parse");
        assert!(matches!(p.command, SpacesCommand::Inspect));
        assert_eq!(p.slug.as_deref(), Some("oncall"));
        assert!(matches!(p.tool, Some(InspectTool::Read)));
        assert_eq!(
            p.tool_args
                .as_ref()
                .and_then(|a| a.get("path").and_then(|v| v.as_str())),
            Some("runbooks/foo.md")
        );
    }

    #[test]
    fn spaces_inspect_parses_grep_tool() {
        let p: SpacesParams = parse_params(args(serde_json::json!({
            "command": "inspect",
            "slug": "oncall",
            "tool": "grep",
            "tool_args": { "pattern": "TODO", "output_mode": "content", "-n": true },
        })))
        .expect("parse");
        assert!(matches!(p.command, SpacesCommand::Inspect));
        assert!(matches!(p.tool, Some(InspectTool::Grep)));
    }

    #[test]
    fn spaces_inspect_parses_glob_tool() {
        let p: SpacesParams = parse_params(args(serde_json::json!({
            "command": "inspect",
            "slug": "oncall",
            "tool": "glob",
            "tool_args": { "pattern": "**/*.md" },
        })))
        .expect("parse");
        assert!(matches!(p.tool, Some(InspectTool::Glob)));
    }

    #[test]
    fn spaces_inspect_parses_ls_tool() {
        let p: SpacesParams = parse_params(args(serde_json::json!({
            "command": "inspect",
            "slug": "oncall",
            "tool": "ls",
            "tool_args": { "path": "" },
        })))
        .expect("parse");
        assert!(matches!(p.tool, Some(InspectTool::Ls)));
    }

    #[test]
    fn spaces_write_parses_args() {
        let p: SpacesParams = parse_params(args(serde_json::json!({
            "command": "write",
            "slug": "oncall",
            "path": "notes/x.md",
            "content": "hello",
        })))
        .expect("parse");
        assert!(matches!(p.command, SpacesCommand::Write));
        assert_eq!(p.path.as_deref(), Some("notes/x.md"));
        assert_eq!(p.content.as_deref(), Some("hello"));
    }

    #[test]
    fn spaces_delete_parses_args() {
        let p: SpacesParams = parse_params(args(serde_json::json!({
            "command": "delete",
            "slug": "oncall",
            "path": "notes/x.md",
        })))
        .expect("parse");
        assert!(matches!(p.command, SpacesCommand::Delete));
        assert_eq!(p.path.as_deref(), Some("notes/x.md"));
    }

    #[test]
    fn spaces_move_parses_args() {
        let p: SpacesParams = parse_params(args(serde_json::json!({
            "command": "move",
            "slug": "oncall",
            "from": "a.md",
            "to": "b.md",
        })))
        .expect("parse");
        assert!(matches!(p.command, SpacesCommand::Move));
        assert_eq!(p.from.as_deref(), Some("a.md"));
        assert_eq!(p.to.as_deref(), Some("b.md"));
    }

    #[test]
    fn tools_includes_spaces() {
        assert!(TOOLS.iter().any(|t| t.name == "spaces"));
    }
}
