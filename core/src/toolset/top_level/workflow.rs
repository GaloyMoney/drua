use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use serde::Deserialize;

use crate::audit::Audit;
use crate::auth::{AuthResource, AuthSubject, AuthVerb};
use crate::primitives::{WorkflowDefinitionId, WorkflowRunId};
use crate::sandbox::{SandboxAgentMode, SandboxMode, SandboxSpecs};
use crate::workflow::{
    StepResult, WorkflowDefinition, WorkflowRun, WorkflowRunState, WorkflowSandboxDecl,
    WorkflowStepDef, WorkflowTrigger, Workflows,
};
use crate::workspace::Workspaces;

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
    Runs {
        definition_id: WorkflowDefinitionId,
    },
    Run {
        run_id: WorkflowRunId,
    },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WorkflowSandboxParam {
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
}

#[derive(Deserialize)]
struct WorkflowStepParam {
    name: String,
    skill: String,
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
        let (name, mode, cpu, memory, disk_size) = match self {
            WorkflowSandboxParam::Scratch {
                name,
                cpu,
                memory,
                disk_size,
            } => (name, SandboxMode::Scratch, cpu, memory, disk_size),
            WorkflowSandboxParam::Repo {
                name,
                repo_url,
                branch,
                cpu,
                memory,
                disk_size,
            } => (
                name,
                SandboxMode::Repo { repo_url, branch },
                cpu,
                memory,
                disk_size,
            ),
        };
        let specs = match (cpu, memory, disk_size) {
            (Some(cpu), Some(memory), Some(disk_size)) => Some(SandboxSpecs {
                cpu,
                memory,
                disk_size,
            }),
            _ => None,
        };
        WorkflowSandboxDecl { name, mode, specs }
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
    workspace_id: String,
    /// `"manual"` or `"webhook"`.
    trigger_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    trigger_provider: Option<String>,
    steps: Vec<WorkflowStepOutput>,
    sandboxes: Vec<WorkflowSandboxOutput>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct WorkflowSandboxOutput {
    name: String,
    /// `"scratch"` or `"repo"`.
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
    workspace_id: String,
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
    workspaces: Arc<Workspaces>,
    /// `None` renders the webhook URL as a path only.
    public_host: Option<String>,
}

impl WorkflowTool {
    pub fn new(
        workflows: Arc<Workflows>,
        workspaces: Arc<Workspaces>,
        public_host: Option<String>,
    ) -> Self {
        Self {
            workflows,
            workspaces,
            public_host,
        }
    }
}

static WORKFLOW_OUTPUT_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<WorkflowOutput>);

static WORKFLOW_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "enum": ["create", "list", "get", "trigger", "runs", "run"],
                "description": "Which workflow operation to perform. `runs` lists runs (truncated outputs); `run` returns a single run with full per-step output."
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
                "description": "Single-step shorthand: NAME of an existing skill in this workspace (create skill first via the `skill` tool). Used only when `steps` is omitted/empty."
            },
            "steps": {
                "type": "array",
                "description": "Multi-step form (create). Takes precedence over the single-step shorthand. Each entry needs `name` and `skill`; optional `sandbox`, `sandbox_mode`, `timeout_seconds`.",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "skill": {
                            "type": "string",
                            "description": "NAME of an existing skill in this workspace (created via the `skill` tool). NOT an inline body — the runtime looks up the skill by this name at trigger time."
                        },
                        "sandbox": {
                            "type": "string",
                            "description": "Name of a sandbox declared in this workflow's top-level `sandboxes` array."
                        },
                        "sandbox_mode": { "type": "string", "enum": ["read", "write"] },
                        "timeout_seconds": { "type": "integer", "minimum": 1 }
                    },
                    "required": ["name", "skill"]
                }
            },
            "sandbox": {
                "type": "string",
                "description": "Optional sandbox name to attach to the agent step (create). Must reference an entry in `sandboxes`."
            },
            "sandboxes": {
                "type": "array",
                "description": "Sandboxes declared by this workflow. Each entry has `kind` (scratch or repo) and a `name`; repo entries also take `repo_url` and optional `branch`. Optional `cpu`/`memory`/`disk_size` override defaults.",
                "items": {
                    "type": "object",
                    "properties": {
                        "kind": { "type": "string", "enum": ["scratch", "repo"] },
                        "name": { "type": "string" },
                        "repo_url": { "type": "string" },
                        "branch": { "type": "string" },
                        "cpu": { "type": "string" },
                        "memory": { "type": "string" },
                        "disk_size": { "type": "string" }
                    },
                    "required": ["kind", "name"]
                }
            },
            "timeout_seconds": {
                "type": "integer",
                "minimum": 1,
                "description": "Per-step timeout in seconds (create). Defaults to 300."
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
            "payload": {
                "description": "Trigger context payload (trigger). Defaults to {}."
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
         optional `payload`), `runs` (requires `definition_id`; truncated \
         step outputs), `run` (requires `run_id`; full per-step outputs)."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &WORKFLOW_SCHEMA
    }

    fn output_schema(&self) -> Option<&serde_json::Value> {
        Some(&WORKFLOW_OUTPUT_SCHEMA)
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.workspace_id().is_some_and(|ws| {
            subject
                .can(AuthVerb::Read, AuthResource::Workflow(ws, None))
                .is_ok()
        })
    }

    fn composable(&self) -> bool {
        false
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let workspace_id = subject.workspace_id().ok_or(ToolSetsError::Unauthorized)?;
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

                let workspace_name = self
                    .workspaces
                    .find_by_id(subject, workspace_id)
                    .await
                    .map(|w| w.name)
                    .map_err(|e| ToolSetsError::Workspace(e.to_string()))?;

                let sandbox_decls: Vec<WorkflowSandboxDecl> =
                    sandboxes.into_iter().map(|s| s.into_decl()).collect();

                let definition = self
                    .workflows
                    .create(
                        subject,
                        workspace_id,
                        &workspace_name,
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
                    .list_for_workspace(subject, workspace_id)
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
                let text = format!(
                    "Workflow run started.\n  run_id:        {}\n  definition_id: {}\n  state:         {}\n\nInspect with: workflow command=run run_id={}",
                    run.id,
                    run.definition_id,
                    run_state_str(run.state),
                    run.id,
                );
                let out = WorkflowOutput {
                    command: "trigger".to_string(),
                    run: Some(run_to_output(&run)),
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
        };

        let structured = serde_json::to_value(&out).expect("WorkflowOutput serialization");
        let mut result = CallToolResult::success(vec![Content::text(text)]);
        result.structured_content = Some(structured);
        Ok(result)
    }
}

fn definition_to_output(d: &WorkflowDefinition) -> WorkflowDefinitionOutput {
    let (trigger_type, trigger_provider) = match &d.trigger {
        WorkflowTrigger::Manual => ("manual".to_string(), None),
        WorkflowTrigger::Webhook { provider, .. } => ("webhook".to_string(), provider.clone()),
    };
    WorkflowDefinitionOutput {
        id: d.id.to_string(),
        name: d.name.clone(),
        description: d.description.clone(),
        workspace_id: d.workspace_id.to_string(),
        trigger_type,
        trigger_provider,
        steps: d.steps.iter().map(step_to_output).collect(),
        sandboxes: d.sandboxes.iter().map(sandbox_to_output).collect(),
    }
}

fn sandbox_to_output(d: &WorkflowSandboxDecl) -> WorkflowSandboxOutput {
    let (kind, repo_url, branch) = match &d.mode {
        SandboxMode::Scratch => ("scratch".to_string(), None, None),
        SandboxMode::Repo { repo_url, branch } => {
            ("repo".to_string(), Some(repo_url.clone()), branch.clone())
        }
    };
    let (cpu, memory, disk_size) = match &d.specs {
        Some(s) => (
            Some(s.cpu.clone()),
            Some(s.memory.clone()),
            Some(s.disk_size.clone()),
        ),
        None => (None, None, None),
    };
    WorkflowSandboxOutput {
        name: d.name.clone(),
        kind,
        repo_url,
        branch,
        cpu,
        memory,
        disk_size,
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
        workspace_id: r.workspace_id.to_string(),
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
            WorkflowTrigger::Manual => (None, None),
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
            let kind = match &sb.mode {
                SandboxMode::Scratch => "scratch".to_string(),
                SandboxMode::Repo { repo_url, branch } => match branch {
                    Some(b) => format!("repo({repo_url}@{b})"),
                    None => format!("repo({repo_url})"),
                },
            };
            out.push_str(&format!("  - {} kind={kind}\n", sb.name));
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
    if runs.iter().any(|r| r.state == WorkflowRunState::Failed) {
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

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}
