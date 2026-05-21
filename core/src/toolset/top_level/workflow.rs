use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use serde::Deserialize;

use crate::audit::Audit;
use crate::auth::{AuthResource, AuthSubject, AuthVerb};
use crate::primitives::{WorkflowDefinitionId, WorkflowRunId};
use crate::project::Projects;
use crate::sandbox::{SandboxAgentMode, SandboxMode, SandboxSpecs};
use crate::workflow::{
    StepResult, WorkflowDefinition, WorkflowRun, WorkflowRunState, WorkflowSandboxDecl,
    WorkflowStepDef, WorkflowTrigger, Workflows,
};

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{parse_params, schema_for};

#[derive(Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum WorkflowParams {
    Create {
        name: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        provider: Option<String>,
        #[serde(default)]
        manual: bool,
        /// Bare CEL boolean evaluated against `trigger` before a run
        /// is created. Memo 019e20a2.
        #[serde(default)]
        trigger_condition: Option<String>,
        /// Multi-step form. When non-empty this takes precedence over
        /// the single-step shorthand (`skill`/`sandbox`/`sandbox_mode`/
        /// `timeout_seconds`).
        #[serde(default)]
        steps: Vec<WorkflowStepParam>,
        /// Single-step shorthand: required only when `steps` is empty.
        #[serde(default)]
        skill: Option<String>,
        #[serde(default)]
        sandbox: Option<String>,
        #[serde(default)]
        sandbox_mode: Option<SandboxAgentMode>,
        #[serde(default)]
        timeout_seconds: Option<u64>,
        /// Declared sandboxes for the workflow. Each step's `sandbox`
        /// (if set) must reference one of these by name.
        #[serde(default)]
        sandboxes: Vec<WorkflowSandboxParam>,
        /// Workflow-wide chain override. Per-step `model_chain` (in
        /// `WorkflowStepParam`) wins; both fall through to the
        /// role/config default when unset.
        #[serde(default)]
        model_chain: Option<llm::ModelChain>,
    },
    List,
    Get {
        definition_id: WorkflowDefinitionId,
    },
    Trigger {
        /// Either the definition's UUID or its project-scoped name.
        /// UUID parsing is tried first; on failure the value is
        /// resolved against the project's `list_for_project`.
        definition_id: String,
        #[serde(default)]
        payload: Option<serde_json::Value>,
    },
    Runs {
        definition_id: WorkflowDefinitionId,
    },
    Run {
        run_id: WorkflowRunId,
    },
    Update {
        definition_id: WorkflowDefinitionId,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        description: Option<String>,
        /// `true` clears `description` to `None`. Ignored unless set.
        #[serde(default)]
        clear_description: bool,
        /// When non-empty AND `update_steps`, replaces the step list.
        #[serde(default)]
        steps: Vec<WorkflowStepParam>,
        #[serde(default)]
        update_steps: bool,
        /// When `update_sandboxes`, replaces the sandbox decl list (use empty array to clear).
        #[serde(default)]
        sandboxes: Vec<WorkflowSandboxParam>,
        #[serde(default)]
        update_sandboxes: bool,
        /// `true` rebuilds the trigger from `provider`/`manual`.
        #[serde(default)]
        update_trigger: bool,
        #[serde(default)]
        provider: Option<String>,
        #[serde(default)]
        manual: bool,
        /// Bare CEL boolean evaluated against `trigger` before a run
        /// is created. Applies when `update_trigger=true`. Memo
        /// 019e20a2.
        #[serde(default)]
        trigger_condition: Option<String>,
        /// Replace the workflow-wide chain. `clear_model_chain: true`
        /// clears it to `None`; otherwise omitting leaves untouched.
        #[serde(default)]
        model_chain: Option<llm::ModelChain>,
        #[serde(default)]
        clear_model_chain: bool,
    },
    Delete {
        definition_id: WorkflowDefinitionId,
    },
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WorkflowSandboxParam {
    /// Provision a fresh empty sandbox for this workflow run.
    Scratch {
        name: String,
        #[serde(default)]
        config: Option<ScratchParamConfig>,
    },
    /// Provision a sandbox that clones a git repo at run start.
    Repo {
        name: String,
        config: RepoParamConfig,
    },
    /// Reference an existing sandbox in the workflow's project by
    /// name. The executor attaches but never provisions, restarts, or
    /// suspends it; the user owns the lifecycle.
    Preexisting { name: String },
}

#[derive(Deserialize, Default, schemars::JsonSchema)]
struct ScratchParamConfig {
    #[serde(default)]
    cpu: Option<String>,
    #[serde(default)]
    memory: Option<String>,
    #[serde(default)]
    disk_size: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct RepoParamConfig {
    repo_url: String,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    cpu: Option<String>,
    #[serde(default)]
    memory: Option<String>,
    #[serde(default)]
    disk_size: Option<String>,
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

/// Tagged step input. `type: agent_step` (the historical default,
/// inferred when omitted) reuses the existing AgentStep shape; `type:
/// tool_step` dispatches a single top-level MCP tool.
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WorkflowStepParam {
    AgentStep {
        name: String,
        /// NAME of an existing skill in this project (created via the
        /// `skill` tool). NOT an inline body — the runtime looks up the
        /// skill by this name at trigger time.
        skill: String,
        /// Name of a sandbox declared in this workflow's top-level
        /// `sandboxes` array.
        #[serde(default)]
        sandbox: Option<String>,
        #[serde(default)]
        sandbox_mode: Option<SandboxAgentMode>,
        #[serde(default)]
        timeout_seconds: Option<u64>,
        #[serde(default)]
        model_chain: Option<llm::ModelChain>,
        /// JSON Schema (root must be `type: object`) for the step's
        /// structured output. Omit to fall back to the default
        /// `{success, output, reason}` schema.
        #[serde(default)]
        output_schema: Option<serde_json::Value>,
        /// Bare CEL boolean expression — when present and false, the
        /// step is skipped (run continues to the next step). Evaluated
        /// against `(trigger, steps)` like `${{ … }}` substitution.
        #[serde(default)]
        condition: Option<String>,
    },
    ToolStep {
        name: String,
        /// Top-level tool name (e.g. `"workflow"`). Must be
        /// `composable: true`. Dispatched with `AuthSubject::WorkflowExecutor`.
        tool: String,
        /// Pre-substitution params; `${{ trigger.X }}` and
        /// `${{ steps.<name>.outputs.Y }}` resolve at run time.
        #[serde(default)]
        params: serde_json::Value,
        #[serde(default)]
        timeout_seconds: Option<u64>,
        /// See `AgentStep::condition` — same semantics for ToolSteps.
        #[serde(default)]
        condition: Option<String>,
    },
}

impl WorkflowStepParam {
    fn into_step(self) -> Result<WorkflowStepDef, ToolSetsError> {
        match self {
            WorkflowStepParam::AgentStep {
                name,
                skill,
                sandbox,
                sandbox_mode,
                timeout_seconds,
                model_chain,
                output_schema,
                condition,
            } => {
                let output_schema = match output_schema {
                    Some(value) => serde_json::from_value(value).map_err(|e| {
                        ToolSetsError::MissingArgument(format!(
                            "step '{name}': output_schema invalid (root must be `type: object` per MCP): {e}"
                        ))
                    })?,
                    None => crate::workflow::default_output_schema(),
                };
                Ok(WorkflowStepDef::AgentStep {
                    name,
                    skill,
                    sandbox,
                    sandbox_mode,
                    timeout_seconds,
                    model_chain,
                    output_schema: Box::new(output_schema),
                    condition,
                })
            }
            WorkflowStepParam::ToolStep {
                name,
                tool,
                params,
                timeout_seconds,
                condition,
            } => Ok(WorkflowStepDef::ToolStep {
                name,
                tool,
                params,
                timeout_seconds,
                condition,
            }),
        }
    }
}

impl WorkflowSandboxParam {
    fn into_decl(self) -> WorkflowSandboxDecl {
        match self {
            WorkflowSandboxParam::Preexisting { name } => WorkflowSandboxDecl::Preexisting { name },
            WorkflowSandboxParam::Scratch { name, config } => {
                let cfg = config.unwrap_or_default();
                WorkflowSandboxDecl::Provisioned {
                    name,
                    mode: SandboxMode::Scratch,
                    specs: specs_from_parts(cfg.cpu, cfg.memory, cfg.disk_size),
                }
            }
            WorkflowSandboxParam::Repo { name, config } => WorkflowSandboxDecl::Provisioned {
                name,
                mode: SandboxMode::Repo {
                    repo_url: config.repo_url,
                    branch: config.branch,
                },
                specs: specs_from_parts(config.cpu, config.memory, config.disk_size),
            },
        }
    }
}

impl WorkflowParams {
    fn audit_action(&self) -> &'static str {
        match self {
            Self::Create { .. } => "workflow.create",
            Self::List => "workflow.list",
            Self::Get { .. } => "workflow.get",
            Self::Trigger { .. } => "workflow.trigger",
            Self::Runs { .. } => "workflow.runs",
            Self::Run { .. } => "workflow.run",
            Self::Update { .. } => "workflow.update",
            Self::Delete { .. } => "workflow.delete",
        }
    }
}

/// Union output across subcommands; only `command` is always set.
#[derive(Default, serde::Serialize, schemars::JsonSchema)]
struct WorkflowOutput {
    /// Which command was executed.
    command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    definition: Option<WorkflowDefinitionOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    definitions: Option<Vec<WorkflowDefinitionOutput>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    run: Option<WorkflowRunOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    runs: Option<Vec<WorkflowRunOutput>>,
    /// Surfaced on `create` only — operators configure the upstream
    /// with this value, then it's never returned again.
    #[serde(skip_serializing_if = "Option::is_none")]
    webhook_secret: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    webhook_url: Option<String>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct WorkflowDefinitionOutput {
    id: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    project_id: String,
    /// `"manual"`, `"webhook"`, or `"cron"`.
    trigger_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    trigger_provider: Option<String>,
    /// Cron expression when `trigger_type == "cron"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    cron_schedule: Option<String>,
    /// IANA timezone when `trigger_type == "cron"`. Defaults to UTC.
    #[serde(skip_serializing_if = "Option::is_none")]
    cron_timezone: Option<String>,
    /// RFC3339 timestamp of the next scheduled fire for cron triggers.
    #[serde(skip_serializing_if = "Option::is_none")]
    next_run_at: Option<String>,
    steps: Vec<WorkflowStepOutput>,
    sandboxes: Vec<WorkflowSandboxOutput>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct WorkflowSandboxOutput {
    name: String,
    /// `"scratch"`, `"repo"`, or `"preexisting"`.
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cpu: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    disk_size: Option<String>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct WorkflowStepOutput {
    name: String,
    /// `"agent_step"` or `"tool_step"`.
    step_type: String,
    /// Empty string for `tool_step`.
    skill: String,
    /// Set on `tool_step`.
    #[serde(skip_serializing_if = "Option::is_none")]
    tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sandbox: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_seconds: Option<u64>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct WorkflowRunOutput {
    id: String,
    definition_id: String,
    project_id: String,
    /// `pending` / `running` / `succeeded` / `failed`.
    state: String,
    started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed_at: Option<String>,
    step_results: Vec<StepResultOutput>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct StepResultOutput {
    name: String,
    /// `null` while pending or after failure.
    output: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed_at: Option<String>,
    /// `Some(<cel-body>)` when the step was skipped because its
    /// `condition:` evaluated to false. Mutually exclusive with
    /// `output` and `error`.
    #[serde(skip_serializing_if = "Option::is_none")]
    skipped: Option<String>,
}

pub struct WorkflowTool {
    workflows: Arc<Workflows>,
    projects: Arc<Projects>,
    /// `None` renders the webhook URL as a path only.
    public_host: Option<String>,
}

impl WorkflowTool {
    pub fn new(
        workflows: Arc<Workflows>,
        projects: Arc<Projects>,
        public_host: Option<String>,
    ) -> Self {
        Self {
            workflows,
            projects,
            public_host,
        }
    }
}

static WORKFLOW_OUTPUT_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<WorkflowOutput>);

/// Schema for an array `items:` slot — derives from the rust type so
/// the schema and the deserializer can never disagree (the bug from
/// the third smoke test was a hand-written `kind` vs `#[serde(tag =
/// "type")]` drift). Strips schemars' root-level `title` /
/// `additionalProperties` since both apply at the surrounding-object
/// level, not inside an item entry.
fn item_schema_for<T: schemars::JsonSchema>() -> serde_json::Value {
    let settings = schemars::gen::SchemaSettings::draft07().with(|s| {
        s.inline_subschemas = true;
        s.meta_schema = None;
    });
    let generator = settings.into_generator();
    let schema = generator.into_root_schema_for::<T>();
    let mut value = serde_json::to_value(schema).expect("schema serialization");
    if let Some(obj) = value.as_object_mut() {
        obj.remove("title");
        obj.remove("$schema");
    }
    value
}

static WORKFLOW_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "enum": ["create", "list", "get", "trigger", "runs", "run", "update", "delete"],
                "description": "Which workflow operation to perform. `trigger` returns immediately with the freshly-spawned run. `runs` lists runs (truncated outputs); `run` returns a single run with full per-step output."
            },
            "name": {
                "type": "string",
                "description": "Workflow display name (create)."
            },
            "description": {
                "type": "string",
                "description": "Optional human-readable description (create)."
            },
            "provider": {
                "type": "string",
                "description": "Webhook provider tag, e.g. 'honeycomb' (create). Omit for a generic Bearer-token webhook."
            },
            "manual": {
                "type": "boolean",
                "description": "Opt out of webhooks entirely and create a manually-triggered workflow (create)."
            },
            "skill": {
                "type": "string",
                "description": "Single-step shorthand: NAME of an existing skill in this project (create skill first via the `skill` tool). Used only when `steps` is omitted/empty."
            },
            "steps": {
                "type": "array",
                "description": "Multi-step form (create). Takes precedence over the single-step shorthand. Each entry needs `name` and `skill`; optional `sandbox`, `sandbox_mode`, `timeout_seconds`.",
                "items": item_schema_for::<WorkflowStepParam>(),
            },
            "sandbox": {
                "type": "string",
                "description": "Optional sandbox name to attach to the agent step (create). Must reference an entry in `sandboxes`."
            },
            "sandboxes": {
                "type": "array",
                "description": "Sandboxes declared by this workflow. Each entry is a tagged variant: `{type:\"scratch\",name}`, `{type:\"repo\",name,config:{repo_url,branch?,cpu?,memory?,disk_size?}}`, or `{type:\"preexisting\",name}` to attach a sandbox the user already created.",
                "items": item_schema_for::<WorkflowSandboxParam>(),
            },
            "timeout_seconds": {
                "type": "integer",
                "minimum": 1,
                "description": "Per-step timeout in seconds (create). Default: 300."
            },
            "definition_id": {
                "type": "string",
                "description": "Workflow definition ID — UUID for `get`/`runs`/`update`, UUID-or-name for `trigger` (the project-scoped workflow name resolves identically)."
            },
            "run_id": {
                "type": "string",
                "format": "uuid",
                "description": "Workflow run ID (run)."
            },
            "payload": {
                "description": "Trigger context payload (trigger). Defaults to {}."
            },
            "clear_description": {
                "type": "boolean",
                "description": "Update only: clears `description` to null. Ignored when false."
            },
            "update_steps": {
                "type": "boolean",
                "description": "Update only: replace the step list with the supplied `steps` array."
            },
            "update_sandboxes": {
                "type": "boolean",
                "description": "Update only: replace the sandbox decl list with the supplied `sandboxes` array."
            },
            "update_trigger": {
                "type": "boolean",
                "description": "Update only: rebuild the trigger from `provider`/`manual`."
            },
            "model_chain": {
                "description": "Workflow-wide model chain override. Per-step `model_chain` (in `steps[]`) wins. On update, omitting leaves untouched; pair with `clear_model_chain: true` to clear.",
                "type": "object",
                "properties": {
                    "primary": item_schema_for::<llm::ModelSpec>(),
                    "fallbacks": {
                        "type": "array",
                        "items": item_schema_for::<llm::ModelSpec>(),
                    }
                },
                "required": ["primary"]
            },
            "clear_model_chain": {
                "type": "boolean",
                "description": "Update only: clears `model_chain` to null. Ignored when false."
            }
        },
        "required": ["command"],
        "additionalProperties": false
    })
});

/// Resolve a `definition_id` field that may be either a UUID or a
/// project-scoped workflow name. UUID parsing wins; the name path is
/// the fallback so callers can write `workflow.trigger
/// definition_id="drua-test-failure"` without doing a `list` lookup
/// first (memo `019e01a4` open Q1).
async fn resolve_definition_id_or_name(
    workflows: &Arc<Workflows>,
    subject: &AuthSubject,
    project_id: crate::primitives::ProjectId,
    raw: &str,
) -> Result<WorkflowDefinitionId, ToolSetsError> {
    if let Ok(uuid) = uuid::Uuid::parse_str(raw) {
        return Ok(WorkflowDefinitionId::from(uuid));
    }
    let definitions = workflows
        .list_for_project(subject, project_id)
        .await
        .map_err(|e| ToolSetsError::Workflow(e.to_string()))?;
    definitions
        .iter()
        .find(|d| d.name == raw)
        .map(|d| d.id)
        .ok_or_else(|| {
            ToolSetsError::MissingArgument(format!(
                "no workflow named '{raw}' in this project (and `{raw}` is not a valid UUID)"
            ))
        })
}

#[async_trait::async_trait]
impl TopLevelTool for WorkflowTool {
    fn name(&self) -> &str {
        "workflow"
    }

    fn description(&self) -> &str {
        "Manage webhook-triggered workflows that run agent skills end-to-end. \
         Each step spawns a fresh agent that has tools `bash`, `text_editor`, \
         `ls`, `grep`, `glob`, `read`, plus the declared sandbox attached — \
         skill bodies should describe the goal in terms of those tools (NOT \
         as raw shell scripts). The trigger payload is interpolated into \
         each step's skill via `$ARGUMENTS`. Commands: `create` (requires \
         `name`; either `steps` array or single-step shorthand `skill`; \
         optional `provider`, `sandboxes`, `manual`, `model_chain`), \
         `list`, `get` (requires `definition_id`), `trigger` (requires \
         `definition_id`, optional `payload`; returns immediately with \
         the spawned run), `runs` (requires `definition_id`; truncated step \
         outputs), `run` (requires `run_id`; full per-step outputs), \
         `update` (requires `definition_id`; optional `name`, \
         `description`+`clear_description`, `steps`+`update_steps`, \
         `sandboxes`+`update_sandboxes`, `provider`/`manual`+`update_trigger`, \
         `model_chain`+`clear_model_chain`), \
         `delete` (requires `definition_id`; cascades to runs and queues \
         a `DeleteFile` on the canonical YAML)."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &WORKFLOW_SCHEMA
    }

    fn inner_output_schema(&self) -> Option<&serde_json::Value> {
        Some(&WORKFLOW_OUTPUT_SCHEMA)
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.project_id().is_some_and(|project| {
            subject
                .can(AuthVerb::Read, AuthResource::Workflow(project, None))
                .is_ok()
        })
    }

    fn composable(&self) -> bool {
        true
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let project_id = subject.project_id().ok_or(ToolSetsError::Unauthorized)?;
        let params: WorkflowParams = parse_params(arguments)?;

        Audit::record_action(params.audit_action());

        let (text, out) = match params {
            WorkflowParams::Create {
                name,
                description,
                provider,
                manual,
                trigger_condition,
                steps,
                skill,
                sandbox,
                sandbox_mode,
                timeout_seconds,
                sandboxes,
                model_chain,
            } => {
                let trigger = if manual {
                    WorkflowTrigger::Manual {
                        condition: trigger_condition.clone(),
                    }
                } else {
                    // Empty secret → Workflows::create generates one.
                    WorkflowTrigger::Webhook {
                        provider: provider.clone(),
                        secret: String::new(),
                        condition: trigger_condition.clone(),
                    }
                };

                let resolved_steps: Vec<WorkflowStepDef> = if !steps.is_empty() {
                    steps
                        .into_iter()
                        .map(WorkflowStepParam::into_step)
                        .collect::<Result<_, _>>()?
                } else {
                    let skill = skill.ok_or_else(|| {
                        ToolSetsError::MissingArgument(
                            "either `steps` or `skill` is required for create".into(),
                        )
                    })?;
                    vec![WorkflowStepDef::AgentStep {
                        name: "step".into(),
                        skill,
                        sandbox,
                        sandbox_mode,
                        timeout_seconds,
                        model_chain: None,
                        output_schema: Box::new(crate::workflow::default_output_schema()),
                        condition: None,
                    }]
                };

                let project_name = self
                    .projects
                    .find_by_id(subject, project_id)
                    .await
                    .map(|w| w.name)?;

                let sandbox_decls: Vec<WorkflowSandboxDecl> =
                    sandboxes.into_iter().map(|s| s.into_decl()).collect();

                let definition = self
                    .workflows
                    .create(
                        subject,
                        project_id,
                        &project_name,
                        name,
                        description,
                        trigger,
                        resolved_steps,
                        sandbox_decls,
                        model_chain,
                    )
                    .await
                    .map_err(|e| ToolSetsError::Workflow(e.to_string()))?;

                let (webhook_url, webhook_secret) = self.webhook_url_and_secret(&definition);
                let text = self.format_create_text(&definition, &webhook_url, &webhook_secret);
                let out = WorkflowOutput {
                    command: "create".to_string(),
                    definition: Some(definition_to_output(&definition)),
                    webhook_url,
                    webhook_secret,
                    ..Default::default()
                };
                (text, out)
            }

            WorkflowParams::List => {
                let definitions = self
                    .workflows
                    .list_for_project(subject, project_id)
                    .await
                    .map_err(|e| ToolSetsError::Workflow(e.to_string()))?;
                let text = format_list_text(&definitions);
                let out = WorkflowOutput {
                    command: "list".to_string(),
                    definitions: Some(definitions.iter().map(definition_to_output).collect()),
                    ..Default::default()
                };
                (text, out)
            }

            WorkflowParams::Get { definition_id } => {
                let definition = self
                    .workflows
                    .find_by_id(subject, definition_id)
                    .await
                    .map_err(|e| ToolSetsError::Workflow(e.to_string()))?;
                let text = format_get_text(&definition);
                let out = WorkflowOutput {
                    command: "get".to_string(),
                    definition: Some(definition_to_output(&definition)),
                    ..Default::default()
                };
                (text, out)
            }

            WorkflowParams::Trigger {
                definition_id,
                payload,
            } => {
                let resolved_id = resolve_definition_id_or_name(
                    &self.workflows,
                    subject,
                    project_id,
                    &definition_id,
                )
                .await?;
                let payload =
                    payload.unwrap_or_else(|| serde_json::Value::Object(Default::default()));
                let maybe_run = self
                    .workflows
                    .trigger_run(subject, resolved_id, payload)
                    .await
                    .map_err(|e| ToolSetsError::Workflow(e.to_string()))?;
                let (text, run_out) = match &maybe_run {
                    Some(run) => (format_run_text(run), Some(run_to_output(run))),
                    None => (
                        "Trigger condition evaluated to false; no run created.".to_string(),
                        None,
                    ),
                };
                let out = WorkflowOutput {
                    command: "trigger".to_string(),
                    run: run_out,
                    ..Default::default()
                };
                (text, out)
            }

            WorkflowParams::Runs { definition_id } => {
                let runs = self
                    .workflows
                    .list_runs(subject, definition_id)
                    .await
                    .map_err(|e| ToolSetsError::Workflow(e.to_string()))?;
                let text = format_runs_text(&runs);
                let out = WorkflowOutput {
                    command: "runs".to_string(),
                    runs: Some(runs.iter().map(run_to_output).collect()),
                    ..Default::default()
                };
                (text, out)
            }

            WorkflowParams::Run { run_id } => {
                let run = self
                    .workflows
                    .find_run_by_id(subject, run_id)
                    .await
                    .map_err(|e| ToolSetsError::Workflow(e.to_string()))?;
                let text = format_run_text(&run);
                let out = WorkflowOutput {
                    command: "run".to_string(),
                    run: Some(run_to_output(&run)),
                    ..Default::default()
                };
                (text, out)
            }

            WorkflowParams::Update {
                definition_id,
                name,
                description,
                clear_description,
                steps,
                update_steps,
                sandboxes,
                update_sandboxes,
                update_trigger,
                provider,
                manual,
                trigger_condition,
                model_chain,
                clear_model_chain,
            } => {
                let description: Option<Option<String>> = if clear_description {
                    Some(None)
                } else {
                    description.filter(|s| !s.is_empty()).map(Some)
                };
                let trigger = if update_trigger {
                    Some(if manual {
                        WorkflowTrigger::Manual {
                            condition: trigger_condition.clone(),
                        }
                    } else {
                        WorkflowTrigger::Webhook {
                            provider,
                            secret: String::new(),
                            condition: trigger_condition.clone(),
                        }
                    })
                } else {
                    None
                };
                let steps_arg = if update_steps {
                    Some(
                        steps
                            .into_iter()
                            .map(WorkflowStepParam::into_step)
                            .collect::<Result<Vec<_>, _>>()?,
                    )
                } else {
                    None
                };
                let sandboxes_arg = update_sandboxes
                    .then(|| sandboxes.into_iter().map(|s| s.into_decl()).collect());
                let model_chain_arg = if clear_model_chain {
                    Some(None)
                } else {
                    model_chain.map(Some)
                };

                let definition = self
                    .workflows
                    .update(
                        subject,
                        definition_id,
                        name,
                        description,
                        trigger,
                        steps_arg,
                        sandboxes_arg,
                        model_chain_arg,
                    )
                    .await
                    .map_err(|e| ToolSetsError::Workflow(e.to_string()))?;
                let text = format_get_text(&definition);
                let out = WorkflowOutput {
                    command: "update".to_string(),
                    definition: Some(definition_to_output(&definition)),
                    ..Default::default()
                };
                (text, out)
            }

            WorkflowParams::Delete { definition_id } => {
                self.workflows
                    .delete(subject, definition_id)
                    .await
                    .map_err(|e| ToolSetsError::Workflow(e.to_string()))?;
                let text = format!("Workflow deleted (id {definition_id}).");
                let out = WorkflowOutput {
                    command: "delete".to_string(),
                    ..Default::default()
                };
                (text, out)
            }
        };

        let structured = serde_json::to_value(&out).expect("WorkflowOutput serialization");
        let mut result = CallToolResult::success(vec![Content::text(text)]);
        result.structured_content = Some(structured);
        Ok(result)
    }
}

fn definition_to_output(d: &WorkflowDefinition) -> WorkflowDefinitionOutput {
    let mut cron_schedule = None;
    let mut cron_timezone = None;
    let mut next_run_at = None;
    let (trigger_type, trigger_provider) = match &d.trigger {
        WorkflowTrigger::Manual { .. } => ("manual".to_string(), None),
        WorkflowTrigger::Webhook { provider, .. } => ("webhook".to_string(), provider.clone()),
        WorkflowTrigger::Cron {
            schedule, timezone, ..
        } => {
            cron_schedule = Some(schedule.clone());
            cron_timezone = timezone.clone();
            next_run_at = d
                .trigger
                .next_fire_at(chrono::Utc::now())
                .ok()
                .flatten()
                .map(|t| t.to_rfc3339());
            ("cron".to_string(), None)
        }
    };
    WorkflowDefinitionOutput {
        id: d.id.to_string(),
        name: d.name.clone(),
        description: d.description.clone(),
        project_id: d.project_id.to_string(),
        trigger_type,
        trigger_provider,
        cron_schedule,
        cron_timezone,
        next_run_at,
        steps: d.steps.iter().map(step_to_output).collect(),
        sandboxes: d.sandboxes.iter().map(sandbox_to_output).collect(),
    }
}

fn sandbox_to_output(d: &WorkflowSandboxDecl) -> WorkflowSandboxOutput {
    match d {
        WorkflowSandboxDecl::Preexisting { name } => WorkflowSandboxOutput {
            name: name.clone(),
            kind: "preexisting".to_string(),
            repo_url: None,
            branch: None,
            cpu: None,
            memory: None,
            disk_size: None,
        },
        WorkflowSandboxDecl::Provisioned { name, mode, specs } => {
            let (kind, repo_url, branch) = match mode {
                SandboxMode::Scratch => ("scratch".to_string(), None, None),
                SandboxMode::Repo { repo_url, branch } => {
                    ("repo".to_string(), Some(repo_url.clone()), branch.clone())
                }
            };
            let (cpu, memory, disk_size) = match specs {
                Some(s) => (
                    Some(s.cpu.clone()),
                    Some(s.memory.clone()),
                    Some(s.disk_size.clone()),
                ),
                None => (None, None, None),
            };
            WorkflowSandboxOutput {
                name: name.clone(),
                kind,
                repo_url,
                branch,
                cpu,
                memory,
                disk_size,
            }
        }
    }
}

fn step_to_output(s: &WorkflowStepDef) -> WorkflowStepOutput {
    match s {
        WorkflowStepDef::AgentStep {
            name,
            skill,
            sandbox,
            timeout_seconds,
            ..
        } => WorkflowStepOutput {
            name: name.clone(),
            step_type: "agent_step".to_string(),
            skill: skill.clone(),
            sandbox: sandbox.clone(),
            timeout_seconds: *timeout_seconds,
            tool: None,
        },
        WorkflowStepDef::ToolStep {
            name,
            tool,
            timeout_seconds,
            ..
        } => WorkflowStepOutput {
            name: name.clone(),
            step_type: "tool_step".to_string(),
            skill: String::new(),
            sandbox: None,
            timeout_seconds: *timeout_seconds,
            tool: Some(tool.clone()),
        },
        WorkflowStepDef::Wait { name, .. } => WorkflowStepOutput {
            name: name.clone(),
            step_type: "wait".to_string(),
            skill: String::new(),
            sandbox: None,
            timeout_seconds: None,
            tool: None,
        },
    }
}

fn run_to_output(r: &WorkflowRun) -> WorkflowRunOutput {
    WorkflowRunOutput {
        id: r.id.to_string(),
        definition_id: r.definition_id.to_string(),
        project_id: r.project_id.to_string(),
        state: run_state_str(r.state).to_string(),
        started_at: r.started_at().to_rfc3339(),
        completed_at: r.completed_at.map(|t| t.to_rfc3339()),
        step_results: r.step_results.iter().map(step_result_to_output).collect(),
    }
}

fn step_result_to_output(sr: &StepResult) -> StepResultOutput {
    StepResultOutput {
        name: sr.name.clone(),
        output: sr.output.clone(),
        error: sr.error.clone(),
        completed_at: sr.completed_at.map(|t| t.to_rfc3339()),
        skipped: sr.skipped.clone(),
    }
}

fn run_state_str(state: WorkflowRunState) -> &'static str {
    match state {
        WorkflowRunState::Pending => "pending",
        WorkflowRunState::Running => "running",
        WorkflowRunState::WaitingForEvent => "waiting_for_event",
        WorkflowRunState::Succeeded => "succeeded",
        WorkflowRunState::Failed => "failed",
        WorkflowRunState::Errored => "errored",
    }
}

impl WorkflowTool {
    fn webhook_url_and_secret(&self, def: &WorkflowDefinition) -> (Option<String>, Option<String>) {
        match &def.trigger {
            WorkflowTrigger::Manual { .. } | WorkflowTrigger::Cron { .. } => (None, None),
            WorkflowTrigger::Webhook { secret, .. } => {
                let url = match &self.public_host {
                    Some(host) => format!("{host}/webhooks/{}", def.id),
                    None => format!("/webhooks/{}", def.id),
                };
                (Some(url), Some(secret.clone()))
            }
        }
    }

    fn format_create_text(
        &self,
        def: &WorkflowDefinition,
        webhook_url: &Option<String>,
        webhook_secret: &Option<String>,
    ) -> String {
        let trigger_label = match &def.trigger {
            WorkflowTrigger::Manual { .. } => "manual".to_string(),
            WorkflowTrigger::Webhook { provider, .. } => match provider {
                Some(p) => format!("webhook ({p})"),
                None => "webhook".to_string(),
            },
            WorkflowTrigger::Cron {
                schedule, timezone, ..
            } => match timezone {
                Some(tz) => format!("cron ({schedule} {tz})"),
                None => format!("cron ({schedule})"),
            },
        };

        let mut out = String::new();
        out.push_str("Workflow created.\n");
        out.push_str(&format!("  id:             {}\n", def.id));
        out.push_str(&format!("  name:           {}\n", def.name));
        out.push_str(&format!("  trigger:        {trigger_label}\n"));
        if let Some(url) = webhook_url {
            let suffix = if self.public_host.is_none() {
                " (set public host in config to render full URL)"
            } else {
                ""
            };
            out.push_str(&format!("  webhook_url:    {url}{suffix}\n"));
        }
        if let Some(s) = webhook_secret {
            out.push_str(&format!("  webhook_secret: {s}\n"));
        }
        out.push_str("  status:         enabled\n");
        if matches!(
            def.trigger,
            WorkflowTrigger::Webhook { provider: Some(ref p), .. } if p == "honeycomb"
        ) {
            out.push_str(
                "\nConfigure your Honeycomb webhook recipient with the URL and secret above.\n",
            );
        }
        out
    }
}

fn format_list_text(defs: &[WorkflowDefinition]) -> String {
    if defs.is_empty() {
        return "No workflows found.".to_string();
    }

    let mut lines = Vec::with_capacity(defs.len() + 2);
    lines.push(format!(
        "{:<38} {:<24} {:<14} {}",
        "ID", "NAME", "TRIGGER", "STEPS"
    ));
    lines.push("-".repeat(96));

    for d in defs {
        let trigger = match &d.trigger {
            WorkflowTrigger::Manual { .. } => "manual".to_string(),
            WorkflowTrigger::Webhook { provider, .. } => match provider {
                Some(p) => format!("webhook:{p}"),
                None => "webhook".to_string(),
            },
            WorkflowTrigger::Cron { schedule, .. } => format!("cron:{schedule}"),
        };
        lines.push(format!(
            "{:<38} {:<24} {:<14} {}",
            d.id,
            truncate(&d.name, 24),
            truncate(&trigger, 14),
            d.steps.len()
        ));
    }
    lines.join("\n")
}

fn format_get_text(d: &WorkflowDefinition) -> String {
    let trigger = match &d.trigger {
        WorkflowTrigger::Manual { .. } => "manual".to_string(),
        WorkflowTrigger::Webhook {
            provider, secret, ..
        } => {
            let label = match provider {
                Some(p) => format!("webhook ({p})"),
                None => "webhook".to_string(),
            };
            format!("{label}\n  webhook_secret: {secret}")
        }
        trigger @ WorkflowTrigger::Cron {
            schedule, timezone, ..
        } => {
            let mut s = format!("cron\n  schedule:    {schedule}");
            let tz_label = timezone.as_deref().unwrap_or("UTC");
            s.push_str(&format!("\n  timezone:    {tz_label}"));
            if let Ok(Some(next)) = trigger.next_fire_at(chrono::Utc::now()) {
                s.push_str(&format!("\n  next_run_at: {}", next.to_rfc3339()));
            }
            s
        }
    };
    let mut out = String::new();
    out.push_str(&format!("id:          {}\n", d.id));
    out.push_str(&format!("name:        {}\n", d.name));
    if let Some(desc) = &d.description {
        out.push_str(&format!("description: {desc}\n"));
    }
    out.push_str(&format!("trigger:     {trigger}\n"));
    if !d.sandboxes.is_empty() {
        out.push_str(&format!("sandboxes:   {}\n", d.sandboxes.len()));
        for sb in &d.sandboxes {
            let kind = match sb {
                WorkflowSandboxDecl::Preexisting { .. } => "preexisting".to_string(),
                WorkflowSandboxDecl::Provisioned { mode, .. } => match mode {
                    SandboxMode::Scratch => "scratch".to_string(),
                    SandboxMode::Repo { repo_url, branch } => match branch {
                        Some(b) => format!("repo({repo_url}@{b})"),
                        None => format!("repo({repo_url})"),
                    },
                },
            };
            out.push_str(&format!("  - {} kind={kind}\n", sb.name()));
        }
    }
    out.push_str(&format!("steps:       {}\n", d.steps.len()));
    for step in &d.steps {
        match step {
            WorkflowStepDef::AgentStep {
                name,
                skill,
                sandbox,
                sandbox_mode,
                timeout_seconds,
                model_chain,
                ..
            } => {
                out.push_str(&format!(
                    "  - agent_step name={name} skill={skill} sandbox={sandbox:?} sandbox_mode={sandbox_mode:?} timeout_s={timeout_seconds:?} model_chain={}\n",
                    model_chain
                        .as_ref()
                        .map(|c| format!("{}+{}", c.primary.name, c.fallbacks.len()))
                        .unwrap_or_else(|| "<inherit>".into())
                ));
            }
            WorkflowStepDef::ToolStep {
                name,
                tool,
                timeout_seconds,
                ..
            } => {
                out.push_str(&format!(
                    "  - tool_step name={name} tool={tool} timeout_s={timeout_seconds:?}\n"
                ));
            }
            WorkflowStepDef::Wait { name, provider, .. } => {
                out.push_str(&format!("  - wait name={name} provider={provider}\n"));
            }
        }
    }
    out
}

fn format_runs_text(runs: &[WorkflowRun]) -> String {
    if runs.is_empty() {
        return "No runs found.".to_string();
    }
    let mut lines = Vec::with_capacity(runs.len() + 2);
    lines.push(format!(
        "{:<38} {:<12} {:<28} {}",
        "RUN_ID", "STATE", "STARTED_AT", "OUTPUT"
    ));
    lines.push("-".repeat(120));
    for r in runs {
        let started = r.started_at().format("%Y-%m-%d %H:%M:%SZ").to_string();
        let failed_step = r.step_results.iter().find(|sr| sr.error.is_some());
        // Errors get a longer truncation cap (200 vs 60 for normal output)
        // so context-window/api errors stay legible in the listing.
        let (output, max_chars) = if let Some(sr) = failed_step {
            let err = sr.error.as_deref().unwrap_or("unknown");
            (format!("FAILED [{}]: {err}", sr.name), 200)
        } else {
            let text = r
                .step_results
                .last()
                .and_then(|sr| {
                    sr.output.as_ref().map(|v| match v {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                })
                .unwrap_or_else(|| "—".to_string());
            (text, 60)
        };
        lines.push(format!(
            "{:<38} {:<12} {:<28} {}",
            r.id,
            run_state_str(r.state),
            started,
            truncate(&output.replace('\n', " "), max_chars)
        ));
    }
    if runs.iter().any(|r| {
        matches!(
            r.state,
            WorkflowRunState::Failed | WorkflowRunState::Errored
        )
    }) {
        lines.push(String::new());
        lines.push("Inspect a run with: workflow command=run run_id=<id>".to_string());
    }
    lines.join("\n")
}

fn format_run_text(r: &WorkflowRun) -> String {
    let mut out = String::new();
    out.push_str(&format!("run_id:        {}\n", r.id));
    out.push_str(&format!("definition_id: {}\n", r.definition_id));
    out.push_str(&format!("state:         {}\n", run_state_str(r.state)));
    out.push_str(&format!("started_at:    {}\n", r.started_at().to_rfc3339()));
    if let Some(t) = r.completed_at {
        out.push_str(&format!("completed_at:  {}\n", t.to_rfc3339()));
    }
    out.push('\n');
    if matches!(
        r.state,
        WorkflowRunState::Pending | WorkflowRunState::Running
    ) {
        out.push_str(&format!(
            "Poll status: workflow command=run run_id={}\n\n",
            r.id
        ));
    }
    if r.step_results.is_empty() {
        out.push_str("No step results.\n");
        return out;
    }
    for (i, sr) in r.step_results.iter().enumerate() {
        let state = if sr.error.is_some() {
            "FAILED"
        } else if sr.skipped.is_some() {
            "SKIPPED"
        } else if sr.completed_at.is_some() {
            "OK"
        } else {
            "RUNNING"
        };
        out.push_str(&format!("--- step {}: {} [{state}] ---\n", i + 1, sr.name));
        if let Some(t) = sr.completed_at {
            out.push_str(&format!("completed_at: {}\n", t.to_rfc3339()));
        }
        if let Some(err) = &sr.error {
            out.push_str(&format!("error: {err}\n"));
        }
        if let Some(body) = &sr.skipped {
            out.push_str(&format!("condition (false): {body}\n"));
        }
        if let Some(v) = &sr.output {
            let text = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            out.push_str("output:\n");
            out.push_str(&text);
            if !text.ends_with('\n') {
                out.push('\n');
            }
        }
        out.push('\n');
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}
