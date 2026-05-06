//! `AdminToolSet` — admin-only tools exposed exclusively through the
//! searchable catalog (`search_tools` → `describe_tool` → `call_tool`).
//!
//! Consolidated into 8 tools with command discriminators:
//! `agent`, `sandbox`, `log`, `project`, `spaces`, `workflow`, `skill`, `note`.
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
use crate::note::{Note, Notes};
use crate::primitives::{
    AgentId, NoteId, ProjectId, SandboxId, SkillId, UserId, WorkflowDefinitionId,
};
use crate::project::{Project, Projects};
use crate::sandbox::{Sandbox, SandboxAgentMode, SandboxMode, SandboxSpecs, Sandboxes};
use crate::skill::{ScopedSkill, Skill, SkillSource, Skills};
use crate::space_fs::SpaceFs;
use crate::workflow::{
    WorkflowDefinition, WorkflowSandboxDecl, WorkflowStepDef, WorkflowTrigger, Workflows,
};

use super::super::error::ToolSetsError;
use super::super::inspect::{dispatch_edit, dispatch_view, parse_view_range, EditOp, ReadOp};
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
    Delete,
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
    /// ID of the agent (required for `attach_sandbox`, `detach_sandbox`, and `delete`).
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

    op: Option<ReadOp>,
    #[serde(default)]
    op_args: Option<JsonObject>,
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
    /// Read-only file ops on a space. `op` selects the sub-tool
    /// (read|ls|grep|glob); `op_args` shape depends on it.
    View,
    /// Mutating file ops on a space. `op` selects the sub-tool
    /// (write|str_replace|insert|delete|move); `op_args` shape
    /// depends on it.
    Edit,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct SpacesParams {
    command: SpacesCommand,
    /// Slug for `create`, `get`, `mount`, `unmount`, `view`, `edit`.
    /// Must match `[a-z0-9-]+` with no leading / trailing / double hyphens.
    slug: Option<String>,
    /// Optional human-readable summary, used by `create`.
    description: Option<String>,
    /// Required for `mount` and `unmount`. The project to attach the
    /// space to (or detach from).
    #[schemars(with = "Option<uuid::Uuid>")]
    project_id: Option<ProjectId>,

    /// Required for `view`. Selects read|ls|grep|glob.
    view_op: Option<ReadOp>,
    /// Required for `edit`. Selects write|str_replace|insert|delete|move.
    edit_op: Option<EditOp>,
    /// Per-op arguments. See command docs for shape.
    #[serde(default)]
    op_args: Option<JsonObject>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum WorkflowCommand {
    Create,
    List,
    Get,
    Update,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct WorkflowStepParams {
    name: String,
    /// NAME of an existing skill in the target project.
    skill: String,
    #[serde(default)]
    sandbox: Option<String>,
    #[serde(default)]
    sandbox_mode: Option<SandboxAgentMode>,
    #[serde(default)]
    timeout_seconds: Option<u64>,
    #[serde(default)]
    model_chain: Option<llm::ModelChain>,
    /// JSON Schema (root must be `type: object`) for this step's
    /// structured output. Omit to fall back to the default `{success,
    /// reason}` schema. Surfaced to the agent as the `submit_output`
    /// tool's `input_schema`.
    #[serde(default)]
    output_schema: Option<serde_json::Value>,
}

impl WorkflowStepParams {
    fn into_step(self) -> Result<WorkflowStepDef, ToolSetsError> {
        let output_schema = match self.output_schema {
            Some(value) => serde_json::from_value(value).map_err(|e| {
                ToolSetsError::MissingArgument(format!(
                    "step '{}': output_schema invalid (root must be `type: object` per MCP): {e}",
                    self.name
                ))
            })?,
            None => crate::workflow::default_output_schema(),
        };
        Ok(WorkflowStepDef::AgentStep {
            name: self.name,
            skill: self.skill,
            sandbox: self.sandbox,
            sandbox_mode: self.sandbox_mode,
            timeout_seconds: self.timeout_seconds,
            model_chain: self.model_chain,
            output_schema,
        })
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WorkflowSandboxParams {
    Scratch {
        name: String,
        #[serde(default)]
        cpu: Option<String>,
        #[serde(default)]
        memory: Option<String>,
        #[serde(default)]
        disk_size: Option<String>,
    },
    Repo {
        name: String,
        repo_url: String,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        cpu: Option<String>,
        #[serde(default)]
        memory: Option<String>,
        #[serde(default)]
        disk_size: Option<String>,
    },
    Preexisting {
        name: String,
    },
}

impl WorkflowSandboxParams {
    fn into_decl(self) -> WorkflowSandboxDecl {
        match self {
            WorkflowSandboxParams::Preexisting { name } => {
                WorkflowSandboxDecl::Preexisting { name }
            }
            WorkflowSandboxParams::Scratch {
                name,
                cpu,
                memory,
                disk_size,
            } => WorkflowSandboxDecl::Provisioned {
                name,
                mode: SandboxMode::Scratch,
                specs: specs_from_parts(cpu, memory, disk_size),
            },
            WorkflowSandboxParams::Repo {
                name,
                repo_url,
                branch,
                cpu,
                memory,
                disk_size,
            } => WorkflowSandboxDecl::Provisioned {
                name,
                mode: SandboxMode::Repo { repo_url, branch },
                specs: specs_from_parts(cpu, memory, disk_size),
            },
        }
    }
}

fn specs_from_parts(
    cpu: Option<String>,
    memory: Option<String>,
    disk_size: Option<String>,
) -> Option<SandboxSpecs> {
    match (cpu, memory, disk_size) {
        (Some(cpu), Some(memory), Some(disk_size)) => Some(SandboxSpecs {
            cpu,
            memory,
            disk_size,
        }),
        _ => None,
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
struct WorkflowParams {
    command: WorkflowCommand,

    /// Required for `create`, `list`.
    #[schemars(with = "Option<uuid::Uuid>")]
    project_id: Option<ProjectId>,

    /// Required for `get`, `update`.
    #[schemars(with = "Option<uuid::Uuid>")]
    definition_id: Option<WorkflowDefinitionId>,

    /// `create`: required. `update`: optional rename.
    name: Option<String>,
    description: Option<String>,

    /// `create`: optional. Defaults to a generic webhook trigger if
    /// `manual` is not set. Bare `provider` builds a webhook trigger.
    provider: Option<String>,
    /// `create`: opt out of webhooks (Manual trigger).
    #[serde(default)]
    manual: bool,

    /// `create` / `update`: full step list. Required for create.
    #[serde(default)]
    steps: Vec<WorkflowStepParams>,
    /// `create` / `update`: full sandbox decl list.
    #[serde(default)]
    sandboxes: Vec<WorkflowSandboxParams>,

    /// Per-step `model_chain` (in `steps`) wins.
    #[serde(default)]
    model_chain: Option<llm::ModelChain>,

    /// `update`-only flags: when `false`, the corresponding field is
    /// left untouched. `clear_*` variants set the field to `None`.
    #[serde(default)]
    update_steps: bool,
    #[serde(default)]
    update_sandboxes: bool,
    #[serde(default)]
    clear_description: bool,
    #[serde(default)]
    update_trigger: bool,
    #[serde(default)]
    clear_model_chain: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum SkillCommand {
    Create,
    List,
    Get,
    Update,
    Delete,
    Invoke,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct SkillParams {
    command: SkillCommand,

    /// Required for `create`, `list`, `invoke`.
    #[schemars(with = "Option<uuid::Uuid>")]
    project_id: Option<ProjectId>,

    /// Required for `get`, `update`, `delete`.
    #[schemars(with = "Option<uuid::Uuid>")]
    skill_id: Option<SkillId>,

    /// `create`: required. `update`: optional rename. `invoke`: required (skill name to resolve).
    name: Option<String>,
    /// `create`: required. `update`: optional.
    description: Option<String>,
    /// `create`: required. `update`: optional.
    body: Option<String>,
    /// `invoke`: optional `$ARGUMENTS` substitution string.
    arguments: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum NoteCommand {
    Create,
    List,
    Get,
    Update,
    Pin,
    Unpin,
    Delete,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct NoteParams {
    command: NoteCommand,

    /// Project owner. Required for every command. Space-scoped notes
    /// are managed via the `spaces` tool — the admin `note` surface
    /// covers project notes only.
    #[schemars(with = "Option<uuid::Uuid>")]
    project_id: Option<ProjectId>,

    /// Required for `get`, `update`, `pin`, `unpin`, `delete`.
    #[schemars(with = "Option<uuid::Uuid>")]
    note_id: Option<NoteId>,

    /// `create` / `update`: required.
    title: Option<String>,
    /// `create` / `update`: required.
    content: Option<String>,
    /// `create` / `update`: optional. Defaults to empty.
    #[serde(default)]
    tags: Vec<String>,

    /// `list`: optional cap. Defaults to 50.
    limit: Option<usize>,
}

static AGENT_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<AgentParams>);
static SANDBOX_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<SandboxParams>);
static LOG_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<LogParams>);
static PROJECT_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<ProjectParams>);
static SPACES_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<SpacesParams>);
static WORKFLOW_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<WorkflowParams>);
static SKILL_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<SkillParams>);
static NOTE_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<NoteParams>);

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
                       `detach_sandbox` (requires `agent_id`, `sandbox_id`), \
                       `delete` (requires `agent_id`; soft-deletes the agent, cascades \
                       to its session and detaches any attached sandbox).",
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
        name: "workflow",
        description: "Manage workflows across any project. Commands: \
                       `create` (requires `project_id`, `name`, `steps`; \
                       optional `description`, `provider`, `manual`, \
                       `sandboxes`), \
                       `list` (requires `project_id`), \
                       `get` (requires `definition_id`), \
                       `update` (requires `definition_id`; optional `name`, \
                       `description`+`clear_description` for None semantics, \
                       `provider`+`update_trigger`, `steps`+`update_steps`, \
                       `sandboxes`+`update_sandboxes`).",
        schema: &WORKFLOW_SCHEMA,
    },
    ToolDef {
        name: "skill",
        description: "Manage skills across any project. Commands: \
                       `create` (requires `project_id`, `name`, \
                       `description`, `body`), \
                       `list` (requires `project_id`; returns every skill the \
                       project's agents can invoke — project + global + \
                       mounted-space + sandbox-exported, each tagged with its \
                       source), \
                       `get` (requires `skill_id`, `project_id`; project- or \
                       global-scoped only), \
                       `update` (requires `skill_id`, `project_id`; any of \
                       `name`/`description`/`body`; project-scoped only — for \
                       space-scoped skills use the `spaces` tool), \
                       `delete` (requires `skill_id`, `project_id`; project- or \
                       global-scoped skills only — for space-scoped skills use \
                       the `spaces` tool), \
                       `invoke` (requires `project_id`, `name`; optional \
                       `arguments` for `$ARGUMENTS` substitution; resolves \
                       across mounted-space, project, global, sandbox tiers).",
        schema: &SKILL_SCHEMA,
    },
    ToolDef {
        name: "note",
        description: "Manage project-scoped notes. Space-scoped notes are managed \
                       via the `spaces` tool (their markdown files at \
                       `spaces/<slug>/notes/*.md` flow through reverse-sync). \
                       Pinning is shared across every project that mounts \
                       the owning space — flip via the `spaces` tool's \
                       `edit op=write` after the note's `pinned` frontmatter \
                       is synced (or use the project surface for project notes). \
                       Commands: \
                       `create` (requires `project_id`, `title`, `content`; \
                       optional `tags`), \
                       `list` (requires `project_id`; optional `limit`), \
                       `get` (requires `note_id`, `project_id`), \
                       `update` (requires `note_id`, `project_id`, `title`, \
                       `content`; optional `tags`), \
                       `pin` (requires `note_id`, `project_id`; idempotent), \
                       `unpin` (requires `note_id`, `project_id`; idempotent), \
                       `delete` (requires `note_id`, `project_id`; rejects \
                       space-scoped notes — use the `spaces` tool for those).",
        schema: &NOTE_SCHEMA,
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
    workflows: Arc<Workflows>,
    skills: Arc<Skills>,
    notes: Arc<Notes>,
}

impl AdminToolSet {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        agents: Arc<Agents>,
        sandboxes: Arc<Sandboxes>,
        audit: Arc<Audit>,
        projects: Arc<Projects>,
        spaces: Arc<AuthedSpaces>,
        space_fs: Arc<SpaceFs>,
        workflows: Arc<Workflows>,
        skills: Arc<Skills>,
        notes: Arc<Notes>,
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
            workflows,
            skills,
            notes,
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
            "workflow" => self.workflow(subject, arguments).await,
            "skill" => self.skill(subject, arguments).await,
            "note" => self.note(subject, arguments).await,
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
                    .create_agent(subject, project_id, &name, None, None)
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

            AgentCommand::Delete => {
                let agent_id = params.agent_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("agent_id is required for delete".to_string())
                })?;
                self.agents
                    .delete(subject, agent_id)
                    .await
                    .map_err(|e| ToolSetsError::Agent(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "Agent deleted (id {agent_id})."
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
                let op = params.op.ok_or_else(|| {
                    ToolSetsError::MissingArgument("op is required for inspect".to_string())
                })?;
                let op_args = params.op_args.unwrap_or_default();

                Audit::record_sandbox_id(sandbox_id);
                execute_inspect(subject, &self.sandboxes, sandbox_id, op, op_args).await
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

            SpacesCommand::View => {
                let slug = params.slug.ok_or_else(|| {
                    ToolSetsError::MissingArgument("slug is required for view".to_string())
                })?;
                let op = params.view_op.ok_or_else(|| {
                    ToolSetsError::MissingArgument("view_op is required for view".to_string())
                })?;
                let op_args = params.op_args.unwrap_or_default();
                Audit::record_action("spaces.view");
                dispatch_view(&self.space_fs, subject, &slug, op, op_args).await
            }

            SpacesCommand::Edit => {
                let slug = params.slug.ok_or_else(|| {
                    ToolSetsError::MissingArgument("slug is required for edit".to_string())
                })?;
                let op = params.edit_op.ok_or_else(|| {
                    ToolSetsError::MissingArgument("edit_op is required for edit".to_string())
                })?;
                let op_args = params.op_args.unwrap_or_default();
                Audit::record_action("spaces.edit");
                dispatch_edit(&self.space_fs, subject, &slug, op, op_args).await
            }
        }
    }

    async fn workflow(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: WorkflowParams = parse_params(arguments)?;

        match params.command {
            WorkflowCommand::Create => {
                let project_id = params.project_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("project_id is required for create".to_string())
                })?;
                let name = params.name.ok_or_else(|| {
                    ToolSetsError::MissingArgument("name is required for create".to_string())
                })?;
                if params.steps.is_empty() {
                    return Err(ToolSetsError::MissingArgument(
                        "steps is required for create".to_string(),
                    ));
                }
                Audit::record_action("workflow.create");
                let project = self.projects.find_by_id(subject, project_id).await?;
                let trigger = if params.manual {
                    WorkflowTrigger::Manual
                } else {
                    WorkflowTrigger::Webhook {
                        provider: params.provider.clone(),
                        secret: String::new(),
                    }
                };
                let steps = params
                    .steps
                    .into_iter()
                    .map(|s| s.into_step())
                    .collect::<Result<Vec<_>, _>>()?;
                let sandboxes = params
                    .sandboxes
                    .into_iter()
                    .map(|s| s.into_decl())
                    .collect();
                let definition = self
                    .workflows
                    .create(
                        subject,
                        project_id,
                        &project.name,
                        name,
                        params.description.filter(|s| !s.is_empty()),
                        trigger,
                        steps,
                        sandboxes,
                        params.model_chain,
                    )
                    .await
                    .map_err(|e| ToolSetsError::Workflow(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(
                    format_workflow(&definition, true),
                )]))
            }

            WorkflowCommand::List => {
                let project_id = params.project_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("project_id is required for list".to_string())
                })?;
                Audit::record_action("workflow.list");
                let definitions = self
                    .workflows
                    .list_for_project(subject, project_id)
                    .await
                    .map_err(|e| ToolSetsError::Workflow(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(
                    format_workflows(&definitions),
                )]))
            }

            WorkflowCommand::Get => {
                let definition_id = params.definition_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("definition_id is required for get".to_string())
                })?;
                Audit::record_action("workflow.get");
                let definition = self
                    .workflows
                    .find_by_id(subject, definition_id)
                    .await
                    .map_err(|e| ToolSetsError::Workflow(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(
                    format_workflow(&definition, false),
                )]))
            }

            WorkflowCommand::Update => {
                let definition_id = params.definition_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument(
                        "definition_id is required for update".to_string(),
                    )
                })?;
                Audit::record_action("workflow.update");
                let description: Option<Option<String>> = if params.clear_description {
                    Some(None)
                } else {
                    params.description.filter(|s| !s.is_empty()).map(Some)
                };
                let trigger = if params.update_trigger {
                    Some(if params.manual {
                        WorkflowTrigger::Manual
                    } else {
                        WorkflowTrigger::Webhook {
                            provider: params.provider.clone(),
                            secret: String::new(),
                        }
                    })
                } else {
                    None
                };
                let steps = if params.update_steps {
                    Some(
                        params
                            .steps
                            .into_iter()
                            .map(|s| s.into_step())
                            .collect::<Result<Vec<_>, _>>()?,
                    )
                } else {
                    None
                };
                let sandboxes = if params.update_sandboxes {
                    Some(
                        params
                            .sandboxes
                            .into_iter()
                            .map(|s| s.into_decl())
                            .collect::<Vec<_>>(),
                    )
                } else {
                    None
                };
                let model_chain = if params.clear_model_chain {
                    Some(None)
                } else {
                    params.model_chain.map(Some)
                };
                let definition = self
                    .workflows
                    .update(
                        subject,
                        definition_id,
                        params.name,
                        description,
                        trigger,
                        steps,
                        sandboxes,
                        model_chain,
                    )
                    .await
                    .map_err(|e| ToolSetsError::Workflow(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(
                    format_workflow(&definition, false),
                )]))
            }
        }
    }

    async fn skill(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: SkillParams = parse_params(arguments)?;

        match params.command {
            SkillCommand::Create => {
                let project_id = params.project_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("project_id is required for create".to_string())
                })?;
                let name = params.name.ok_or_else(|| {
                    ToolSetsError::MissingArgument("name is required for create".to_string())
                })?;
                let description = params.description.ok_or_else(|| {
                    ToolSetsError::MissingArgument("description is required for create".to_string())
                })?;
                let body = params.body.ok_or_else(|| {
                    ToolSetsError::MissingArgument("body is required for create".to_string())
                })?;
                Audit::record_action("skill.create");
                let project = self.projects.find_by_id(subject, project_id).await?;
                let skill = self
                    .skills
                    .create(subject, project_id, &project.name, name, description, body)
                    .await
                    .map_err(|e| ToolSetsError::Skill(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(format_skill(
                    &skill, true,
                ))]))
            }

            SkillCommand::List => {
                let project_id = params.project_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("project_id is required for list".to_string())
                })?;
                Audit::record_action("skill.list");
                // Same view as the project skills UI — see Skills::list_for_scope.
                let scoped = self
                    .skills
                    .list_for_scope(subject, project_id)
                    .await
                    .map_err(|e| ToolSetsError::Skill(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(
                    format_scoped_skills(&scoped),
                )]))
            }

            SkillCommand::Get => {
                let skill_id = params.skill_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("skill_id is required for get".to_string())
                })?;
                let project_id = params.project_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("project_id is required for get".to_string())
                })?;
                Audit::record_action("skill.get");
                let skill = self
                    .skills
                    .find_by_id(subject, skill_id, project_id)
                    .await
                    .map_err(|e| ToolSetsError::Skill(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(format_skill(
                    &skill, false,
                ))]))
            }

            SkillCommand::Update => {
                let skill_id = params.skill_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("skill_id is required for update".to_string())
                })?;
                let project_id = params.project_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("project_id is required for update".to_string())
                })?;
                Audit::record_action("skill.update");
                let skill = self
                    .skills
                    .update(
                        subject,
                        skill_id,
                        project_id,
                        params.name,
                        params.description,
                        params.body,
                    )
                    .await
                    .map_err(|e| ToolSetsError::Skill(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(format_skill(
                    &skill, false,
                ))]))
            }

            SkillCommand::Delete => {
                let skill_id = params.skill_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("skill_id is required for delete".to_string())
                })?;
                let project_id = params.project_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("project_id is required for delete".to_string())
                })?;
                Audit::record_action("skill.delete");
                self.skills
                    .delete(subject, skill_id, project_id)
                    .await
                    .map_err(|e| ToolSetsError::Skill(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "Skill deleted (id {skill_id})."
                ))]))
            }

            SkillCommand::Invoke => {
                let project_id = params.project_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("project_id is required for invoke".to_string())
                })?;
                let name = params.name.ok_or_else(|| {
                    ToolSetsError::MissingArgument("name is required for invoke".to_string())
                })?;
                Audit::record_action("skill.invoke");
                let rendered = self
                    .skills
                    .interpolate_skill(&name, Some(project_id), None, params.arguments.as_deref())
                    .await
                    .map_err(|e| ToolSetsError::Skill(e.to_string()))?;
                match rendered {
                    Some(body) => Ok(CallToolResult::success(vec![Content::text(body)])),
                    None => Ok(CallToolResult::error(vec![Content::text(format!(
                        "Unknown skill: {name}"
                    ))])),
                }
            }
        }
    }

    async fn note(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let params: NoteParams = parse_params(arguments)?;

        match params.command {
            NoteCommand::Create => {
                let project_id = params.project_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("project_id is required for create".to_string())
                })?;
                let title = params.title.ok_or_else(|| {
                    ToolSetsError::MissingArgument("title is required for create".to_string())
                })?;
                let content = params.content.ok_or_else(|| {
                    ToolSetsError::MissingArgument("content is required for create".to_string())
                })?;
                Audit::record_action("note.create");
                let project = self.projects.find_by_id(subject, project_id).await?;
                let note = self
                    .notes
                    .store(
                        subject,
                        project_id,
                        &project.name,
                        title,
                        content,
                        params.tags,
                    )
                    .await
                    .map_err(|e| ToolSetsError::Note(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(format_note(
                    &note, true,
                ))]))
            }

            NoteCommand::List => {
                let project_id = params.project_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("project_id is required for list".to_string())
                })?;
                Audit::record_action("note.list");
                let limit = params.limit.unwrap_or(50).clamp(1, 200);
                let notes = self
                    .notes
                    .list(subject, project_id, limit)
                    .await
                    .map_err(|e| ToolSetsError::Note(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(format_notes(
                    &notes,
                ))]))
            }

            NoteCommand::Get => {
                let note_id = params.note_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("note_id is required for get".to_string())
                })?;
                let project_id = params.project_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("project_id is required for get".to_string())
                })?;
                Audit::record_action("note.get");
                let note = self
                    .notes
                    .find_by_id(subject, project_id, note_id)
                    .await
                    .map_err(|e| ToolSetsError::Note(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(format_note(
                    &note, false,
                ))]))
            }

            NoteCommand::Update => {
                let note_id = params.note_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("note_id is required for update".to_string())
                })?;
                let project_id = params.project_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("project_id is required for update".to_string())
                })?;
                let title = params.title.ok_or_else(|| {
                    ToolSetsError::MissingArgument("title is required for update".to_string())
                })?;
                let content = params.content.ok_or_else(|| {
                    ToolSetsError::MissingArgument("content is required for update".to_string())
                })?;
                Audit::record_action("note.update");
                let note = self
                    .notes
                    .update(subject, project_id, note_id, title, content, params.tags)
                    .await
                    .map_err(|e| ToolSetsError::Note(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(format_note(
                    &note, false,
                ))]))
            }

            NoteCommand::Pin => {
                let note_id = params.note_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("note_id is required for pin".to_string())
                })?;
                let project_id = params.project_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("project_id is required for pin".to_string())
                })?;
                Audit::record_action("note.pin");
                let note = self
                    .notes
                    .pin(subject, project_id, note_id)
                    .await
                    .map_err(|e| ToolSetsError::Note(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(format_note(
                    &note, false,
                ))]))
            }

            NoteCommand::Unpin => {
                let note_id = params.note_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("note_id is required for unpin".to_string())
                })?;
                let project_id = params.project_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("project_id is required for unpin".to_string())
                })?;
                Audit::record_action("note.unpin");
                let note = self
                    .notes
                    .unpin(subject, project_id, note_id)
                    .await
                    .map_err(|e| ToolSetsError::Note(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(format_note(
                    &note, false,
                ))]))
            }

            NoteCommand::Delete => {
                let note_id = params.note_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("note_id is required for delete".to_string())
                })?;
                let project_id = params.project_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("project_id is required for delete".to_string())
                })?;
                Audit::record_action("note.delete");
                self.notes
                    .delete(subject, project_id, note_id)
                    .await
                    .map_err(|e| ToolSetsError::Note(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "Note deleted (id {note_id})."
                ))]))
            }
        }
    }
}

async fn execute_inspect(
    sub: &AuthSubject,
    sandboxes: &Sandboxes,
    sandbox_id: SandboxId,
    op: ReadOp,
    op_args: JsonObject,
) -> Result<CallToolResult, ToolSetsError> {
    let is_ls = matches!(op, ReadOp::Ls);

    let ls_ignore: Vec<String> = if is_ls {
        op_args
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

    let req = match op {
        ReadOp::Grep => ExecuteRequest {
            tool: "Grep".to_string(),
            input: serde_json::Value::Object(op_args),
        },
        ReadOp::Glob => ExecuteRequest {
            tool: "Glob".to_string(),
            input: serde_json::Value::Object(op_args),
        },
        ReadOp::Read => build_read_request(&op_args)?,
        ReadOp::Ls => build_ls_request(&op_args)?,
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

fn format_workflow(d: &WorkflowDefinition, created: bool) -> String {
    let header = if created {
        "Workflow created."
    } else {
        "Workflow:"
    };
    let trigger = match &d.trigger {
        WorkflowTrigger::Manual => "manual".to_string(),
        WorkflowTrigger::Webhook { provider, .. } => match provider {
            Some(p) => format!("webhook ({p})"),
            None => "webhook".to_string(),
        },
        WorkflowTrigger::Cron { schedule, timezone } => match timezone {
            Some(tz) => format!("cron ({schedule} {tz})"),
            None => format!("cron ({schedule})"),
        },
    };
    let description = d.description.as_deref().unwrap_or("\u{2014}");
    format!(
        "{header}\n  id: {}\n  name: {}\n  project: {}\n  trigger: {}\n  description: {}\n  steps: {}\n  sandboxes: {}",
        d.id,
        d.name,
        d.project_id,
        trigger,
        description,
        d.steps.len(),
        d.sandboxes.len(),
    )
}

fn format_workflows(defs: &[WorkflowDefinition]) -> String {
    if defs.is_empty() {
        return "No workflows found.".to_string();
    }

    let mut lines = Vec::with_capacity(defs.len() + 2);
    lines.push(format!(
        "{:<38} {:<24} {:<14} {:<6} {}",
        "ID", "NAME", "TRIGGER", "STEPS", "PROJECT"
    ));
    lines.push("-".repeat(110));
    for d in defs {
        let trigger = match &d.trigger {
            WorkflowTrigger::Manual => "manual".to_string(),
            WorkflowTrigger::Webhook { provider, .. } => match provider {
                Some(p) => format!("webhook:{p}"),
                None => "webhook".to_string(),
            },
            WorkflowTrigger::Cron { schedule, .. } => format!("cron:{schedule}"),
        };
        lines.push(format!(
            "{:<38} {:<24} {:<14} {:<6} {}",
            d.id,
            truncate(&d.name, 24),
            truncate(&trigger, 14),
            d.steps.len(),
            d.project_id,
        ));
    }
    lines.join("\n")
}

fn skill_scope_label(s: &Skill) -> &'static str {
    if s.space_id.is_some() {
        "space"
    } else if s.project_id.is_some() {
        "project"
    } else {
        "global"
    }
}

fn format_skill(s: &Skill, created: bool) -> String {
    let header = if created { "Skill created." } else { "Skill:" };
    format!(
        "{header}\n  id: {}\n  name: {}\n  scope: {}\n  description: {}",
        s.id,
        s.name,
        skill_scope_label(s),
        s.description,
    )
}

fn scoped_skill_label(source: &SkillSource) -> String {
    match source {
        SkillSource::Project { .. } => "project".to_string(),
        SkillSource::Global { .. } => "global".to_string(),
        SkillSource::Space { space_slug, .. } => format!("space:{space_slug}"),
        SkillSource::Sandbox { sandbox_name, .. } => format!("sandbox:{sandbox_name}"),
    }
}

fn scoped_skill_id(source: &SkillSource, fallback_name: &str) -> String {
    match source {
        SkillSource::Project { skill_id, .. }
        | SkillSource::Global { skill_id }
        | SkillSource::Space { skill_id, .. } => skill_id.to_string(),
        // Sandbox-exported skills are runtime-only — no DB id. Show
        // the synthetic handle so the row is still addressable in the
        // table.
        SkillSource::Sandbox {
            sandbox_id,
            sandbox_name: _,
        } => format!("sandbox:{sandbox_id}:{fallback_name}"),
    }
}

fn format_scoped_skills(skills: &[ScopedSkill]) -> String {
    if skills.is_empty() {
        return "No skills found.".to_string();
    }
    let mut lines = Vec::with_capacity(skills.len() + 3);
    lines.push(format!(
        "{:<38} {:<24} {:<22} {}",
        "ID", "NAME", "SCOPE", "DESCRIPTION"
    ));
    lines.push("-".repeat(120));
    for s in skills {
        lines.push(format!(
            "{:<38} {:<24} {:<22} {}",
            scoped_skill_id(&s.source, &s.name),
            truncate(&s.name, 24),
            truncate(&scoped_skill_label(&s.source), 22),
            truncate(&s.description, 40),
        ));
    }
    lines.push(String::new());
    lines.push(
        "Only `scope = project` rows are editable here — \
         space-scoped skills are managed via the spaces tool, \
         sandbox-exported skills come from the sandbox itself."
            .to_string(),
    );
    lines.join("\n")
}

fn format_note(n: &Note, created: bool) -> String {
    let header = if created { "Note created." } else { "Note:" };
    let tags = if n.tags().is_empty() {
        "\u{2014}".to_string()
    } else {
        n.tags().join(", ")
    };
    let pinned = if n.is_pinned() { "yes" } else { "no" };
    format!(
        "{header}\n  id: {}\n  title: {}\n  pinned: {}\n  tags: {}",
        n.id,
        n.title(),
        pinned,
        tags,
    )
}

fn format_notes(notes: &[Note]) -> String {
    if notes.is_empty() {
        return "No notes found.".to_string();
    }

    let mut lines = Vec::with_capacity(notes.len() + 2);
    lines.push(format!(
        "{:<38} {:<6} {:<40} {}",
        "ID", "PINNED", "TITLE", "TAGS"
    ));
    lines.push("-".repeat(110));
    for n in notes {
        let tags = if n.tags().is_empty() {
            "\u{2014}".to_string()
        } else {
            n.tags().join(",")
        };
        let pinned = if n.is_pinned() { "yes" } else { "no" };
        lines.push(format!(
            "{:<38} {:<6} {:<40} {}",
            n.id,
            pinned,
            truncate(n.title(), 40),
            truncate(&tags, 40),
        ));
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
        assert!(s.contains("\"view\""));
        assert!(s.contains("\"edit\""));
    }

    #[test]
    fn spaces_view_parses_full_args() {
        let p: SpacesParams = parse_params(args(serde_json::json!({
            "command": "view",
            "slug": "oncall",
            "view_op": "read",
            "op_args": { "path": "runbooks/foo.md" },
        })))
        .expect("parse");
        assert!(matches!(p.command, SpacesCommand::View));
        assert_eq!(p.slug.as_deref(), Some("oncall"));
        assert!(matches!(p.view_op, Some(ReadOp::Read)));
        assert_eq!(
            p.op_args
                .as_ref()
                .and_then(|a| a.get("path").and_then(|v| v.as_str())),
            Some("runbooks/foo.md")
        );
    }

    #[test]
    fn spaces_view_parses_grep_op() {
        let p: SpacesParams = parse_params(args(serde_json::json!({
            "command": "view",
            "slug": "oncall",
            "view_op": "grep",
            "op_args": { "pattern": "TODO", "output_mode": "content", "-n": true },
        })))
        .expect("parse");
        assert!(matches!(p.command, SpacesCommand::View));
        assert!(matches!(p.view_op, Some(ReadOp::Grep)));
    }

    #[test]
    fn spaces_view_parses_glob_op() {
        let p: SpacesParams = parse_params(args(serde_json::json!({
            "command": "view",
            "slug": "oncall",
            "view_op": "glob",
            "op_args": { "pattern": "**/*.md" },
        })))
        .expect("parse");
        assert!(matches!(p.view_op, Some(ReadOp::Glob)));
    }

    #[test]
    fn spaces_view_parses_ls_op() {
        let p: SpacesParams = parse_params(args(serde_json::json!({
            "command": "view",
            "slug": "oncall",
            "view_op": "ls",
            "op_args": { "path": "" },
        })))
        .expect("parse");
        assert!(matches!(p.view_op, Some(ReadOp::Ls)));
    }

    #[test]
    fn spaces_edit_write_parses() {
        let p: SpacesParams = parse_params(args(serde_json::json!({
            "command": "edit",
            "slug": "oncall",
            "edit_op": "write",
            "op_args": { "path": "notes/x.md", "content": "hello" },
        })))
        .expect("parse");
        assert!(matches!(p.command, SpacesCommand::Edit));
        assert!(matches!(p.edit_op, Some(EditOp::Write)));
    }

    #[test]
    fn spaces_edit_str_replace_parses() {
        let p: SpacesParams = parse_params(args(serde_json::json!({
            "command": "edit",
            "slug": "oncall",
            "edit_op": "str_replace",
            "op_args": { "path": "notes/x.md", "old_str": "a", "new_str": "b" },
        })))
        .expect("parse");
        assert!(matches!(p.edit_op, Some(EditOp::StrReplace)));
    }

    #[test]
    fn spaces_edit_insert_parses() {
        let p: SpacesParams = parse_params(args(serde_json::json!({
            "command": "edit",
            "slug": "oncall",
            "edit_op": "insert",
            "op_args": { "path": "notes/x.md", "line": 0, "text": "first line\n" },
        })))
        .expect("parse");
        assert!(matches!(p.edit_op, Some(EditOp::Insert)));
    }

    #[test]
    fn spaces_edit_delete_parses() {
        let p: SpacesParams = parse_params(args(serde_json::json!({
            "command": "edit",
            "slug": "oncall",
            "edit_op": "delete",
            "op_args": { "path": "notes/x.md" },
        })))
        .expect("parse");
        assert!(matches!(p.edit_op, Some(EditOp::Delete)));
    }

    #[test]
    fn spaces_edit_move_parses() {
        let p: SpacesParams = parse_params(args(serde_json::json!({
            "command": "edit",
            "slug": "oncall",
            "edit_op": "move",
            "op_args": { "from": "a.md", "to": "b.md" },
        })))
        .expect("parse");
        assert!(matches!(p.edit_op, Some(EditOp::Move)));
    }

    #[test]
    fn tools_includes_spaces() {
        assert!(TOOLS.iter().any(|t| t.name == "spaces"));
    }

    #[test]
    fn tools_includes_workflow_and_skill() {
        assert!(TOOLS.iter().any(|t| t.name == "workflow"));
        assert!(TOOLS.iter().any(|t| t.name == "skill"));
    }

    #[test]
    fn workflow_create_parses_full_args() {
        let project_id = uuid::Uuid::new_v4();
        let p: WorkflowParams = parse_params(args(serde_json::json!({
            "command": "create",
            "project_id": project_id,
            "name": "nightly",
            "description": "Run nightly checks",
            "manual": true,
            "steps": [
                { "name": "step", "skill": "audit", "timeout_seconds": 60 }
            ],
            "sandboxes": [
                { "type": "scratch", "name": "sb1" }
            ],
        })))
        .expect("parse");
        assert!(matches!(p.command, WorkflowCommand::Create));
        assert_eq!(p.project_id.map(uuid::Uuid::from), Some(project_id));
        assert_eq!(p.name.as_deref(), Some("nightly"));
        assert!(p.manual);
        assert_eq!(p.steps.len(), 1);
        assert_eq!(p.sandboxes.len(), 1);
    }

    #[test]
    fn workflow_list_requires_project() {
        let project_id = uuid::Uuid::new_v4();
        let p: WorkflowParams = parse_params(args(serde_json::json!({
            "command": "list",
            "project_id": project_id,
        })))
        .expect("parse");
        assert!(matches!(p.command, WorkflowCommand::List));
        assert_eq!(p.project_id.map(uuid::Uuid::from), Some(project_id));
    }

    #[test]
    fn workflow_get_takes_definition_id() {
        let definition_id = uuid::Uuid::new_v4();
        let p: WorkflowParams = parse_params(args(serde_json::json!({
            "command": "get",
            "definition_id": definition_id,
        })))
        .expect("parse");
        assert!(matches!(p.command, WorkflowCommand::Get));
        assert_eq!(p.definition_id.map(uuid::Uuid::from), Some(definition_id));
    }

    #[test]
    fn workflow_update_parses_partial_fields() {
        let definition_id = uuid::Uuid::new_v4();
        let p: WorkflowParams = parse_params(args(serde_json::json!({
            "command": "update",
            "definition_id": definition_id,
            "name": "renamed",
            "update_steps": true,
            "steps": [
                { "name": "s1", "skill": "audit" }
            ],
        })))
        .expect("parse");
        assert!(matches!(p.command, WorkflowCommand::Update));
        assert_eq!(p.name.as_deref(), Some("renamed"));
        assert!(p.update_steps);
        assert!(!p.update_sandboxes);
        assert!(!p.update_trigger);
        assert!(!p.clear_description);
    }

    #[test]
    fn workflow_schema_includes_command_enum() {
        let s = serde_json::to_string(&*WORKFLOW_SCHEMA).unwrap();
        for v in ["create", "list", "get", "update"] {
            assert!(s.contains(&format!("\"{v}\"")), "missing {v} in schema");
        }
    }

    #[test]
    fn skill_create_parses_full_args() {
        let project_id = uuid::Uuid::new_v4();
        let p: SkillParams = parse_params(args(serde_json::json!({
            "command": "create",
            "project_id": project_id,
            "name": "review",
            "description": "Review the PR",
            "body": "Review $ARGUMENTS",
        })))
        .expect("parse");
        assert!(matches!(p.command, SkillCommand::Create));
        assert_eq!(p.project_id.map(uuid::Uuid::from), Some(project_id));
        assert_eq!(p.name.as_deref(), Some("review"));
        assert_eq!(p.description.as_deref(), Some("Review the PR"));
        assert_eq!(p.body.as_deref(), Some("Review $ARGUMENTS"));
    }

    #[test]
    fn skill_list_requires_project() {
        let project_id = uuid::Uuid::new_v4();
        let p: SkillParams = parse_params(args(serde_json::json!({
            "command": "list",
            "project_id": project_id,
        })))
        .expect("parse");
        assert!(matches!(p.command, SkillCommand::List));
    }

    #[test]
    fn skill_update_parses_partial_fields() {
        let skill_id = uuid::Uuid::new_v4();
        let project_id = uuid::Uuid::new_v4();
        let p: SkillParams = parse_params(args(serde_json::json!({
            "command": "update",
            "skill_id": skill_id,
            "project_id": project_id,
            "description": "new desc",
        })))
        .expect("parse");
        assert!(matches!(p.command, SkillCommand::Update));
        assert!(p.name.is_none());
        assert_eq!(p.description.as_deref(), Some("new desc"));
        assert!(p.body.is_none());
    }

    #[test]
    fn skill_invoke_parses_with_arguments() {
        let project_id = uuid::Uuid::new_v4();
        let p: SkillParams = parse_params(args(serde_json::json!({
            "command": "invoke",
            "project_id": project_id,
            "name": "alpha-skill",
            "arguments": "PR-123",
        })))
        .expect("parse");
        assert!(matches!(p.command, SkillCommand::Invoke));
        assert_eq!(p.project_id.map(uuid::Uuid::from), Some(project_id));
        assert_eq!(p.name.as_deref(), Some("alpha-skill"));
        assert_eq!(p.arguments.as_deref(), Some("PR-123"));
    }

    #[test]
    fn skill_delete_takes_ids() {
        let skill_id = uuid::Uuid::new_v4();
        let project_id = uuid::Uuid::new_v4();
        let p: SkillParams = parse_params(args(serde_json::json!({
            "command": "delete",
            "skill_id": skill_id,
            "project_id": project_id,
        })))
        .expect("parse");
        assert!(matches!(p.command, SkillCommand::Delete));
        assert_eq!(p.skill_id.map(uuid::Uuid::from), Some(skill_id));
        assert_eq!(p.project_id.map(uuid::Uuid::from), Some(project_id));
    }

    #[test]
    fn skill_schema_includes_command_enum() {
        let s = serde_json::to_string(&*SKILL_SCHEMA).unwrap();
        for v in ["create", "list", "get", "update", "delete", "invoke"] {
            assert!(s.contains(&format!("\"{v}\"")), "missing {v} in schema");
        }
    }

    #[test]
    fn workflow_sandbox_param_repo_into_decl_extracts_repo() {
        let p: WorkflowSandboxParams = serde_json::from_value(serde_json::json!({
            "type": "repo",
            "name": "main",
            "repo_url": "git@github.com:org/r.git",
            "branch": "dev",
        }))
        .expect("parse");
        let decl = p.into_decl();
        match decl {
            WorkflowSandboxDecl::Provisioned {
                name,
                mode: SandboxMode::Repo { repo_url, branch },
                specs,
            } => {
                assert_eq!(name, "main");
                assert_eq!(repo_url, "git@github.com:org/r.git");
                assert_eq!(branch.as_deref(), Some("dev"));
                assert!(specs.is_none());
            }
            other => panic!("unexpected decl: {other:?}"),
        }
    }

    #[test]
    fn tools_includes_note() {
        assert!(TOOLS.iter().any(|t| t.name == "note"));
    }

    #[test]
    fn note_create_parses_full_args() {
        let project_id = uuid::Uuid::new_v4();
        let p: NoteParams = parse_params(args(serde_json::json!({
            "command": "create",
            "project_id": project_id,
            "title": "Onboarding",
            "content": "Welcome to the team.",
            "tags": ["onboarding", "welcome"],
        })))
        .expect("parse");
        assert!(matches!(p.command, NoteCommand::Create));
        assert_eq!(p.project_id.map(uuid::Uuid::from), Some(project_id));
        assert_eq!(p.title.as_deref(), Some("Onboarding"));
        assert_eq!(p.content.as_deref(), Some("Welcome to the team."));
        assert_eq!(
            p.tags,
            vec!["onboarding".to_string(), "welcome".to_string()]
        );
    }

    #[test]
    fn note_list_takes_optional_limit() {
        let project_id = uuid::Uuid::new_v4();
        let p: NoteParams = parse_params(args(serde_json::json!({
            "command": "list",
            "project_id": project_id,
            "limit": 10,
        })))
        .expect("parse");
        assert!(matches!(p.command, NoteCommand::List));
        assert_eq!(p.limit, Some(10));
    }

    #[test]
    fn note_get_takes_ids() {
        let note_id = uuid::Uuid::new_v4();
        let project_id = uuid::Uuid::new_v4();
        let p: NoteParams = parse_params(args(serde_json::json!({
            "command": "get",
            "note_id": note_id,
            "project_id": project_id,
        })))
        .expect("parse");
        assert!(matches!(p.command, NoteCommand::Get));
        assert_eq!(p.note_id.map(uuid::Uuid::from), Some(note_id));
    }

    #[test]
    fn note_update_parses_full_args() {
        let note_id = uuid::Uuid::new_v4();
        let project_id = uuid::Uuid::new_v4();
        let p: NoteParams = parse_params(args(serde_json::json!({
            "command": "update",
            "note_id": note_id,
            "project_id": project_id,
            "title": "Updated title",
            "content": "New body",
            "tags": ["x"],
        })))
        .expect("parse");
        assert!(matches!(p.command, NoteCommand::Update));
        assert_eq!(p.title.as_deref(), Some("Updated title"));
        assert_eq!(p.content.as_deref(), Some("New body"));
        assert_eq!(p.tags, vec!["x".to_string()]);
    }

    #[test]
    fn note_rejects_unknown_command() {
        let res: Result<NoteParams, _> = parse_params(args(serde_json::json!({
            "command": "destroy",
        })));
        assert!(res.is_err());
    }

    #[test]
    fn note_pin_takes_ids() {
        let note_id = uuid::Uuid::new_v4();
        let project_id = uuid::Uuid::new_v4();
        let p: NoteParams = parse_params(args(serde_json::json!({
            "command": "pin",
            "note_id": note_id,
            "project_id": project_id,
        })))
        .expect("parse");
        assert!(matches!(p.command, NoteCommand::Pin));
        assert_eq!(p.note_id.map(uuid::Uuid::from), Some(note_id));
    }

    #[test]
    fn note_unpin_takes_ids() {
        let note_id = uuid::Uuid::new_v4();
        let project_id = uuid::Uuid::new_v4();
        let p: NoteParams = parse_params(args(serde_json::json!({
            "command": "unpin",
            "note_id": note_id,
            "project_id": project_id,
        })))
        .expect("parse");
        assert!(matches!(p.command, NoteCommand::Unpin));
    }

    #[test]
    fn note_schema_includes_command_enum() {
        let s = serde_json::to_string(&*NOTE_SCHEMA).unwrap();
        for v in ["create", "list", "get", "update", "pin", "unpin", "delete"] {
            assert!(s.contains(&format!("\"{v}\"")), "missing {v} in schema");
        }
    }
}
