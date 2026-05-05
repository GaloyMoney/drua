use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use serde::Deserialize;

use crate::audit::Audit;
use crate::auth::{AuthResource, AuthSubject, AuthVerb};
use crate::primitives::{WorkflowDefinitionId, WorkflowRunId};
use crate::project::Projects;
use crate::sandbox::{SandboxAgentMode, SandboxMode, SandboxSpecs};
use crate::workflow::{
    RunStateFilter, RunWithStepDetails, StepAgentDetails, StepResult, WorkflowDefinition,
    WorkflowRun, WorkflowRunState, WorkflowSandboxDecl, WorkflowStepDef, WorkflowTrigger,
    Workflows,
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
    },
    List,
    Get {
        definition_id: WorkflowDefinitionId,
    },
    Trigger {
        definition_id: WorkflowDefinitionId,
        #[serde(default)]
        payload: Option<serde_json::Value>,
    },
    AwaitRun {
        run_id: WorkflowRunId,
        /// Wait budget. Defaults to 360 seconds.
        #[serde(default)]
        timeout_seconds: Option<u64>,
    },
    Runs {
        definition_id: WorkflowDefinitionId,
        #[serde(default)]
        state: Option<RunStateParam>,
        #[serde(default)]
        before: Option<chrono::DateTime<chrono::Utc>>,
        #[serde(default)]
        limit: Option<usize>,
    },
    Run {
        run_id: WorkflowRunId,
        #[serde(default)]
        include: Option<RunIncludeParam>,
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
    },
}

#[derive(Deserialize, schemars::JsonSchema, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum RunStateParam {
    Succeeded,
    Failed,
    Running,
    Any,
}

impl RunStateParam {
    fn into_filter(self) -> Option<RunStateFilter> {
        match self {
            Self::Succeeded => Some(RunStateFilter::Succeeded),
            Self::Failed => Some(RunStateFilter::Failed),
            Self::Running => Some(RunStateFilter::Running),
            Self::Any => None,
        }
    }
}

#[derive(Deserialize, schemars::JsonSchema, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum RunIncludeParam {
    Summary,
    Steps,
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

#[derive(Deserialize, schemars::JsonSchema)]
struct WorkflowStepParam {
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
}

impl WorkflowStepParam {
    fn into_step(self) -> WorkflowStepDef {
        WorkflowStepDef::AgentStep {
            name: self.name,
            skill: self.skill,
            sandbox: self.sandbox,
            sandbox_mode: self.sandbox_mode,
            timeout_seconds: self.timeout_seconds,
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
            Self::AwaitRun { .. } => "workflow.await_run",
            Self::Runs { .. } => "workflow.runs",
            Self::Run { .. } => "workflow.run",
            Self::Update { .. } => "workflow.update",
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
    /// `"agent_step"`.
    step_type: String,
    skill: String,
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
                "enum": ["create", "list", "get", "trigger", "await_run", "runs", "run", "update"],
                "description": "Which workflow operation to perform. `trigger` returns immediately with the freshly-spawned run; pair with `await_run` to block until terminal. `runs` lists runs (truncated outputs); `run` returns a single run with full per-step output."
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
                "description": "Per-step timeout in seconds (create); wait budget in seconds (await_run). Defaults: create=300, await_run=360."
            },
            "definition_id": {
                "type": "string",
                "format": "uuid",
                "description": "Workflow definition ID (get / trigger / runs)."
            },
            "run_id": {
                "type": "string",
                "format": "uuid",
                "description": "Workflow run ID (run)."
            },
            "state": {
                "type": "string",
                "enum": ["succeeded", "failed", "running", "any"],
                "description": "Filter runs by state (runs). `running` matches pending+running. Defaults to `any`."
            },
            "before": {
                "type": "string",
                "format": "date-time",
                "description": "Cursor for `runs`: drop runs whose `started_at` is at or after this RFC3339 timestamp."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "description": "Max runs returned by `runs`. Defaults to 20."
            },
            "include": {
                "type": "string",
                "enum": ["summary", "steps"],
                "description": "`run` projection: `summary` (default — header + per-step state/error/output) or `steps` (adds tool-call telemetry per step: `tool_calls_by_name`, `tool_call_sequence`, model, rounds, tokens). NO transcript, NO tool inputs/outputs — those are session-tier and reached via direct DB."
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
            }
        },
        "required": ["command"],
        "additionalProperties": false
    })
});

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
         optional `provider`, `sandboxes`, `manual`), `list`, `get` \
         (requires `definition_id`), `trigger` (requires `definition_id`, \
         optional `payload`; returns immediately with the spawned run), \
         `await_run` (requires `run_id`; blocks until terminal — pair with \
         `trigger` when you need the final state inline), `runs` \
         (requires `definition_id`; optional `state` filter \
         succeeded/failed/running/any, `before` RFC3339 cursor, `limit` \
         default 20 — tabular header with trigger summary), `run` \
         (requires `run_id`; optional `include`: `summary` (default — \
         header + per-step state/error/output) or `steps` (adds \
         tool-call telemetry per step: `tool_calls_by_name`, \
         `tool_call_sequence`, model, rounds, tokens — NO transcript / \
         tool inputs / message bodies, those are session-tier)), \
         `update` (requires `definition_id`; optional `name`, \
         `description`+`clear_description`, `steps`+`update_steps`, \
         `sandboxes`+`update_sandboxes`, `provider`/`manual`+`update_trigger`)."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &WORKFLOW_SCHEMA
    }

    fn output_schema(&self) -> Option<&serde_json::Value> {
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
                steps,
                skill,
                sandbox,
                sandbox_mode,
                timeout_seconds,
                sandboxes,
            } => {
                let trigger = if manual {
                    WorkflowTrigger::Manual
                } else {
                    // Empty secret → Workflows::create generates one.
                    WorkflowTrigger::Webhook {
                        provider: provider.clone(),
                        secret: String::new(),
                    }
                };

                let resolved_steps: Vec<WorkflowStepDef> = if !steps.is_empty() {
                    steps
                        .into_iter()
                        .map(WorkflowStepParam::into_step)
                        .collect()
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
                let payload =
                    payload.unwrap_or_else(|| serde_json::Value::Object(Default::default()));
                let run = self
                    .workflows
                    .trigger_run(subject, definition_id, payload)
                    .await
                    .map_err(|e| ToolSetsError::Workflow(e.to_string()))?;
                let text = format_run_text(&run);
                let out = WorkflowOutput {
                    command: "trigger".to_string(),
                    run: Some(run_to_output(&run)),
                    ..Default::default()
                };
                (text, out)
            }

            WorkflowParams::AwaitRun {
                run_id,
                timeout_seconds,
            } => {
                let timeout = std::time::Duration::from_secs(timeout_seconds.unwrap_or(360));
                let run = self
                    .workflows
                    .await_run_completion(subject, run_id, Some(timeout))
                    .await
                    .map_err(|e| ToolSetsError::Workflow(e.to_string()))?;
                let text = format_run_text(&run);
                let out = WorkflowOutput {
                    command: "await_run".to_string(),
                    run: Some(run_to_output(&run)),
                    ..Default::default()
                };
                (text, out)
            }

            WorkflowParams::Runs {
                definition_id,
                state,
                before,
                limit,
            } => {
                let filter = state.and_then(RunStateParam::into_filter);
                let runs = self
                    .workflows
                    .list_runs_filtered(subject, definition_id, filter, before, limit)
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

            WorkflowParams::Run { run_id, include } => match include {
                Some(RunIncludeParam::Steps) => {
                    let details = self
                        .workflows
                        .find_run_with_step_details(subject, run_id)
                        .await
                        .map_err(|e| ToolSetsError::Workflow(e.to_string()))?;
                    let text = format_run_with_details_text(&details);
                    let out = WorkflowOutput {
                        command: "run".to_string(),
                        run: Some(run_to_output(&details.run)),
                        ..Default::default()
                    };
                    (text, out)
                }
                _ => {
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
            },

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
            } => {
                let description: Option<Option<String>> = if clear_description {
                    Some(None)
                } else {
                    description.filter(|s| !s.is_empty()).map(Some)
                };
                let trigger = if update_trigger {
                    Some(if manual {
                        WorkflowTrigger::Manual
                    } else {
                        WorkflowTrigger::Webhook {
                            provider,
                            secret: String::new(),
                        }
                    })
                } else {
                    None
                };
                let steps_arg = update_steps.then(|| {
                    steps
                        .into_iter()
                        .map(WorkflowStepParam::into_step)
                        .collect()
                });
                let sandboxes_arg = update_sandboxes
                    .then(|| sandboxes.into_iter().map(|s| s.into_decl()).collect());

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
        WorkflowTrigger::Manual => ("manual".to_string(), None),
        WorkflowTrigger::Webhook { provider, .. } => ("webhook".to_string(), provider.clone()),
        WorkflowTrigger::Cron { schedule, timezone } => {
            cron_schedule = Some(schedule.clone());
            cron_timezone = timezone.clone();
            next_run_at = crate::workflow::next_cron_fire_at(
                schedule,
                timezone.as_deref(),
                chrono::Utc::now(),
            )
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
    }
}

fn run_state_str(state: WorkflowRunState) -> &'static str {
    match state {
        WorkflowRunState::Pending => "pending",
        WorkflowRunState::Running => "running",
        WorkflowRunState::Succeeded => "succeeded",
        WorkflowRunState::Failed => "failed",
    }
}

impl WorkflowTool {
    fn webhook_url_and_secret(&self, def: &WorkflowDefinition) -> (Option<String>, Option<String>) {
        match &def.trigger {
            WorkflowTrigger::Manual | WorkflowTrigger::Cron { .. } => (None, None),
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
            WorkflowTrigger::Manual => "manual".to_string(),
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
        WorkflowTrigger::Manual => "manual".to_string(),
        WorkflowTrigger::Webhook { provider, secret } => {
            let label = match provider {
                Some(p) => format!("webhook ({p})"),
                None => "webhook".to_string(),
            };
            format!("{label}\n  webhook_secret: {secret}")
        }
        WorkflowTrigger::Cron { schedule, timezone } => {
            let mut s = format!("cron\n  schedule:    {schedule}");
            let tz_label = timezone.as_deref().unwrap_or("UTC");
            s.push_str(&format!("\n  timezone:    {tz_label}"));
            if let Ok(Some(next)) = crate::workflow::next_cron_fire_at(
                schedule,
                timezone.as_deref(),
                chrono::Utc::now(),
            ) {
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
            } => {
                out.push_str(&format!(
                    "  - agent_step name={name} skill={skill} sandbox={sandbox:?} sandbox_mode={sandbox_mode:?} timeout_s={timeout_seconds:?}\n"
                ));
            }
        }
    }
    out
}

pub(crate) fn format_runs_text(runs: &[WorkflowRun]) -> String {
    if runs.is_empty() {
        return "No runs found.".to_string();
    }
    let mut lines = Vec::with_capacity(runs.len() + 2);
    lines.push(format!(
        "{:<38} {:<10} {:<20} {:<20} {:<8} {}",
        "RUN_ID", "STATE", "STARTED", "ENDED", "DUR", "TRIGGER_SUMMARY"
    ));
    lines.push("-".repeat(140));
    for r in runs {
        let started = r.started_at().format("%Y-%m-%d %H:%M:%S").to_string();
        let ended = r
            .completed_at
            .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| "—".to_string());
        let dur = match r.completed_at {
            Some(end) => format_duration(end - r.started_at()),
            None => "—".to_string(),
        };
        let trigger = render_trigger_summary(&r.trigger_context);
        lines.push(format!(
            "{:<38} {:<10} {:<20} {:<20} {:<8} {}",
            r.id,
            run_state_str(r.state),
            started,
            ended,
            dur,
            truncate(&trigger.replace('\n', " "), 80)
        ));
    }
    if runs.iter().any(|r| r.state == WorkflowRunState::Failed) {
        lines.push(String::new());
        lines.push(
            "Inspect a run with: workflow command=run run_id=<id> [include=steps]".to_string(),
        );
    }
    lines.join("\n")
}

/// One-line render of the trigger context. Tries to surface a
/// recognisable signature for each provider; falls back to compact
/// JSON when the shape isn't known.
fn render_trigger_summary(ctx: &serde_json::Value) -> String {
    let obj = match ctx.as_object() {
        Some(o) => o,
        None => return ctx.to_string(),
    };
    if obj.is_empty() {
        return "manual".to_string();
    }
    let provider = obj
        .get("provider")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let trigger_name = obj
        .get("trigger")
        .and_then(|v| v.as_object())
        .and_then(|t| t.get("name"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let trigger_id = obj
        .get("trigger")
        .and_then(|v| v.as_object())
        .and_then(|t| t.get("id"))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let mut parts: Vec<String> = Vec::new();
    if let Some(p) = provider.as_ref() {
        parts.push(p.clone());
    }
    if let Some(name) = trigger_name.as_ref() {
        parts.push(name.clone());
    }
    if let Some(groups) = obj.get("result_groups").and_then(|v| v.as_array()) {
        if let Some(first) = groups.first().and_then(|v| v.as_object()) {
            let kv: Vec<String> = first
                .iter()
                .map(|(k, v)| format!("{k}={}", json_scalar(v)))
                .collect();
            if !kv.is_empty() {
                parts.push(kv.join(","));
            }
        }
    }
    if let Some(id) = trigger_id.as_ref() {
        parts.push(id.clone());
    }
    if parts.is_empty() {
        return ctx.to_string();
    }
    parts.join(" / ")
}

fn json_scalar(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

fn format_duration(d: chrono::Duration) -> String {
    let total = d.num_seconds().max(0);
    let m = total / 60;
    let s = total % 60;
    if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

pub(crate) fn format_run_text(r: &WorkflowRun) -> String {
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
            "Block until terminal: workflow command=await_run run_id={}\n\n",
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

/// `--include=steps` projection. `output_text` is the step's return
/// value (= the agent's last assistant turn). No transcript, no tool
/// inputs/outputs — those are session-tier.
pub(crate) fn format_run_with_details_text(d: &RunWithStepDetails) -> String {
    const TOOL_CALL_SEQUENCE_CAP: usize = 200;

    let r = &d.run;
    let mut out = String::new();
    out.push_str(&format!("run_id:        {}\n", r.id));
    out.push_str(&format!("definition_id: {}\n", r.definition_id));
    out.push_str(&format!("project_id:    {}\n", r.project_id));
    out.push_str(&format!("state:         {}\n", run_state_str(r.state)));
    out.push_str(&format!("started_at:    {}\n", r.started_at().to_rfc3339()));
    if let Some(t) = r.completed_at {
        out.push_str(&format!("ended_at:      {}\n", t.to_rfc3339()));
        let dur = (t - r.started_at()).num_milliseconds().max(0);
        out.push_str(&format!("duration_ms:   {dur}\n"));
    }
    out.push_str("\ntrigger_context:\n");
    let ctx_pretty = serde_json::to_string_pretty(&r.trigger_context)
        .unwrap_or_else(|_| r.trigger_context.to_string());
    for line in ctx_pretty.lines() {
        out.push_str(&format!("  {line}\n"));
    }

    let mut total_in: u64 = 0;
    let mut total_out: u64 = 0;
    let mut total_cache_read: u64 = 0;
    for step in &d.steps {
        if let Some(det) = &step.details {
            total_in += det.summary.tokens.input;
            total_out += det.summary.tokens.output;
            total_cache_read += det.summary.tokens.cache_read;
        }
    }
    out.push_str(&format!(
        "\ntokens (run total): input={total_in} output={total_out} cache_read={total_cache_read}\n"
    ));

    out.push_str("\nsteps:\n");
    for (i, step) in d.steps.iter().enumerate() {
        out.push_str(&format!("  - step {} : {}\n", i + 1, step.step_name));
        let state_label = match (step.error.as_ref(), step.completed_at) {
            (Some(_), _) => "failed",
            (None, Some(_)) => "succeeded",
            _ => "running",
        };
        out.push_str(&format!("    state:        {state_label}\n"));
        if let Some(t) = step.completed_at {
            out.push_str(&format!("    completed_at: {}\n", t.to_rfc3339()));
        }
        if let Some(err) = &step.error {
            out.push_str("    error:\n");
            for line in err.lines() {
                out.push_str(&format!("      {line}\n"));
            }
        }
        if let Some(det) = &step.details {
            format_step_details(&mut out, det, TOOL_CALL_SEQUENCE_CAP);
        } else {
            out.push_str("    (no agent session — step did not start an agent)\n");
        }
        if let Some(v) = &step.output {
            let text = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            out.push_str("    output_text: |\n");
            for line in text.lines() {
                out.push_str(&format!("      {line}\n"));
            }
            if text.is_empty() {
                out.push_str("      (empty)\n");
            }
        }
        out.push('\n');
    }
    out
}

fn format_step_details(out: &mut String, det: &StepAgentDetails, sequence_cap: usize) {
    out.push_str(&format!("    agent_id:     {}\n", det.agent_id));
    out.push_str(&format!("    session_id:   {}\n", det.session_id));
    if let Some(model) = &det.summary.model {
        out.push_str(&format!("    model:        {model}\n"));
    }
    out.push_str(&format!("    rounds:       {}\n", det.summary.rounds));
    out.push_str(&format!(
        "    tool_calls_count: {}\n",
        det.summary.tool_calls_count
    ));
    if !det.summary.tool_calls_by_name.is_empty() {
        out.push_str("    tool_calls_by_name:\n");
        for (name, count) in &det.summary.tool_calls_by_name {
            out.push_str(&format!("      {name}: {count}\n"));
        }
    }
    let seq = &det.summary.tool_call_sequence;
    if !seq.is_empty() {
        out.push_str("    tool_call_sequence:\n");
        for entry in seq.iter().take(sequence_cap) {
            out.push_str(&format!(
                "      - {{ ts: {}, name: {} }}\n",
                entry.ts.to_rfc3339(),
                entry.name
            ));
        }
        if seq.len() > sequence_cap {
            out.push_str(&format!(
                "      (... +{} more truncated)\n",
                seq.len() - sequence_cap
            ));
        }
    }
    let t = &det.summary.tokens;
    out.push_str(&format!(
        "    tokens:       input={} output={} cache_read={} cache_write={}\n",
        t.input, t.output, t.cache_read, t.cache_write
    ));
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(value: serde_json::Value) -> WorkflowParams {
        let obj: rmcp::model::JsonObject = serde_json::from_value(value).expect("object");
        super::super::parse_params(Some(obj)).expect("parse")
    }

    #[test]
    fn runs_parses_optional_filters_and_cursor() {
        let definition_id = uuid::Uuid::new_v4();
        let p = parse(serde_json::json!({
            "command": "runs",
            "definition_id": definition_id,
            "state": "failed",
            "before": "2026-05-05T05:50:00Z",
            "limit": 5,
        }));
        match p {
            WorkflowParams::Runs {
                definition_id: d,
                state,
                before,
                limit,
            } => {
                assert_eq!(uuid::Uuid::from(d), definition_id);
                assert!(matches!(state, Some(RunStateParam::Failed)));
                assert!(before.is_some());
                assert_eq!(limit, Some(5));
            }
            other => panic!("expected Runs, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn runs_state_any_resolves_to_no_filter() {
        assert!(RunStateParam::Any.into_filter().is_none());
        assert!(matches!(
            RunStateParam::Failed.into_filter(),
            Some(RunStateFilter::Failed)
        ));
        assert!(matches!(
            RunStateParam::Running.into_filter(),
            Some(RunStateFilter::Running)
        ));
    }

    #[test]
    fn run_parses_include_steps() {
        let run_id = uuid::Uuid::new_v4();
        let p = parse(serde_json::json!({
            "command": "run",
            "run_id": run_id,
            "include": "steps",
        }));
        match p {
            WorkflowParams::Run { run_id: r, include } => {
                assert_eq!(uuid::Uuid::from(r), run_id);
                assert!(matches!(include, Some(RunIncludeParam::Steps)));
            }
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn run_include_defaults_to_none() {
        let run_id = uuid::Uuid::new_v4();
        let p = parse(serde_json::json!({
            "command": "run",
            "run_id": run_id,
        }));
        match p {
            WorkflowParams::Run { include, .. } => assert!(include.is_none()),
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn render_trigger_summary_honeycomb_shape() {
        let ctx = serde_json::json!({
            "provider": "honeycomb",
            "trigger": { "id": "EuZfD1Qbn2x", "name": "lana-stale-pending-jobs" },
            "result_groups": [{ "Result": 2 }]
        });
        let s = render_trigger_summary(&ctx);
        assert!(s.contains("honeycomb"));
        assert!(s.contains("lana-stale-pending-jobs"));
        assert!(s.contains("Result=2"));
        assert!(s.contains("EuZfD1Qbn2x"));
    }

    #[test]
    fn render_trigger_summary_empty_object() {
        let s = render_trigger_summary(&serde_json::json!({}));
        assert_eq!(s, "manual");
    }

    #[test]
    fn schema_includes_new_runs_and_include_props() {
        let s = serde_json::to_string(&*WORKFLOW_SCHEMA).unwrap();
        for token in [
            "\"runs\"",
            "\"run\"",
            "\"include\"",
            "\"state\"",
            "\"before\"",
            "\"limit\"",
            "\"steps\"",
            "\"summary\"",
        ] {
            assert!(s.contains(token), "schema missing {token}");
        }
    }
}
