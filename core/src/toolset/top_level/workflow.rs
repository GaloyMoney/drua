//! `workflow` — consolidated workspace-scoped workflow management.
//!
//! Single tool with a `command` discriminator (mirrors `notes`):
//! `create`, `list`, `get`, `trigger`, `runs`.
//!
//! Read commands (`list`, `get`, `runs`) require `can_read_workspace`;
//! write commands (`create`, `trigger`) enforce `can_write_workspace`
//! inside `call()`.

use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, Content, JsonObject};
use serde::Deserialize;

use crate::audit::Audit;
use crate::auth::AuthSubject;
use crate::primitives::WorkflowDefinitionId;
use crate::workflow::{
    StepResult, WorkflowDefinition, WorkflowRun, WorkflowRunState, WorkflowStepDef,
    WorkflowTrigger, Workflows,
};
use crate::workspace::Workspaces;

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{parse_params, schema_for};

// ---------------------------------------------------------------------------
// Params (tagged enum — no parameter sprawl)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum WorkflowParams {
    /// Create a new workflow definition (MVP: single-step webhook workflows).
    Create {
        name: String,
        #[serde(default)]
        description: Option<String>,
        /// Webhook provider (e.g. `"honeycomb"`). Omit for a generic
        /// `Authorization: Bearer <secret>` shared-token webhook. Pass
        /// `manual: true` to opt out of the webhook entirely.
        #[serde(default)]
        provider: Option<String>,
        #[serde(default)]
        manual: bool,
        skill: String,
        #[serde(default)]
        sandbox: Option<String>,
        #[serde(default)]
        timeout_seconds: Option<u64>,
    },
    /// List workflow definitions in the workspace.
    List,
    /// Get a workflow definition by ID.
    Get { definition_id: WorkflowDefinitionId },
    /// Manually trigger a workflow run.
    Trigger {
        definition_id: WorkflowDefinitionId,
        #[serde(default)]
        payload: Option<serde_json::Value>,
    },
    /// List runs for a workflow definition.
    Runs { definition_id: WorkflowDefinitionId },
}

impl WorkflowParams {
    fn audit_action(&self) -> &'static str {
        match self {
            Self::Create { .. } => "workflow.create",
            Self::List => "workflow.list",
            Self::Get { .. } => "workflow.get",
            Self::Trigger { .. } => "workflow.trigger",
            Self::Runs { .. } => "workflow.runs",
        }
    }
}

// ---------------------------------------------------------------------------
// Output shapes (schemars-derived; also used for serialization)
// ---------------------------------------------------------------------------

/// Union output for all workflow subcommands. Only `command` is required;
/// other fields are populated per subcommand.
#[derive(Default, serde::Serialize, schemars::JsonSchema)]
struct WorkflowOutput {
    /// Which command was executed.
    command: String,
    // -- single definition (create/get) --
    #[serde(skip_serializing_if = "Option::is_none")]
    definition: Option<WorkflowDefinitionOutput>,
    // -- list of definitions (list) --
    #[serde(skip_serializing_if = "Option::is_none")]
    definitions: Option<Vec<WorkflowDefinitionOutput>>,
    // -- single run (trigger) --
    #[serde(skip_serializing_if = "Option::is_none")]
    run: Option<WorkflowRunOutput>,
    // -- list of runs (runs) --
    #[serde(skip_serializing_if = "Option::is_none")]
    runs: Option<Vec<WorkflowRunOutput>>,
    // -- create-only fields --
    /// Auto-generated webhook secret (only set on `create` for webhook
    /// triggers — surfaced once so the caller can configure the upstream).
    #[serde(skip_serializing_if = "Option::is_none")]
    webhook_secret: Option<String>,
    /// Webhook URL (only set on `create` for webhook triggers).
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
    /// Trigger kind: `"manual"` or `"webhook"`.
    trigger_type: String,
    /// Webhook provider tag when `trigger_type` is `"webhook"` (e.g. `"honeycomb"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    trigger_provider: Option<String>,
    steps: Vec<WorkflowStepOutput>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct WorkflowStepOutput {
    name: String,
    /// Step kind: `"agent_step"` for MVP.
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
    /// One of `pending` | `running` | `succeeded` | `failed`.
    state: String,
    started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed_at: Option<String>,
    step_results: Vec<StepResultOutput>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct StepResultOutput {
    name: String,
    /// Step output as JSON (string, object, etc.) — `null` if not yet
    /// completed or if the step failed.
    output: Option<serde_json::Value>,
    /// Error message when the step failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed_at: Option<String>,
}

// ---------------------------------------------------------------------------
// Tool
// ---------------------------------------------------------------------------

pub struct WorkflowTool {
    workflows: Arc<Workflows>,
    workspaces: Arc<Workspaces>,
    /// Optional public-facing host (e.g. `https://drua.example.com`) used
    /// to render the webhook URL on `create` responses. When `None` the
    /// response shows just the path.
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
                "enum": ["create", "list", "get", "trigger", "runs"],
                "description": "Which workflow operation to perform."
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
                "description": "Skill name for the single agent step (create)."
            },
            "sandbox": {
                "type": "string",
                "description": "Optional sandbox name to attach to the agent step (create)."
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
        "Manage webhook-triggered workflows that run an agent skill end-to-end. \
         Commands: `create` (requires `name`, `skill`; optional `provider`, `sandbox`, \
         `timeout_seconds`, `manual`), `list`, `get` (requires `definition_id`), \
         `trigger` (requires `definition_id`, optional `payload`), \
         `runs` (requires `definition_id`)."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &WORKFLOW_SCHEMA
    }

    fn output_schema(&self) -> Option<&serde_json::Value> {
        Some(&WORKFLOW_OUTPUT_SCHEMA)
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.can_read_workspace()
    }

    fn composable(&self) -> bool {
        // Workflows spawn agents under a system subject and run
        // asynchronously — same constraint as `agent` and `sandbox`.
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
                skill,
                sandbox,
                timeout_seconds,
            } => {
                if !subject.can_write_workspace() {
                    return Err(ToolSetsError::Unauthorized);
                }

                let trigger = if manual {
                    WorkflowTrigger::Manual
                } else {
                    // Empty secret → Workflows::create generates one.
                    WorkflowTrigger::Webhook {
                        provider: provider.clone(),
                        secret: String::new(),
                    }
                };

                let step = WorkflowStepDef::AgentStep {
                    name: "step".into(),
                    skill,
                    sandbox,
                    timeout_seconds,
                };

                let workspace_name = self
                    .workspaces
                    .find_by_id(subject, workspace_id)
                    .await
                    .map(|w| w.name)
                    .map_err(|e| ToolSetsError::Workspace(e.to_string()))?;

                let definition = self
                    .workflows
                    .create(
                        subject,
                        workspace_id,
                        &workspace_name,
                        name,
                        description,
                        trigger,
                        vec![step],
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
                if !subject.can_write_workspace() {
                    return Err(ToolSetsError::Unauthorized);
                }
                let payload =
                    payload.unwrap_or_else(|| serde_json::Value::Object(Default::default()));
                let run = self
                    .workflows
                    .trigger_run(subject, definition_id, payload)
                    .await
                    .map_err(|e| ToolSetsError::Workflow(e.to_string()))?;
                let text = format!(
                    "Workflow run started.\n  run_id:        {}\n  definition_id: {}\n  state:         {}",
                    run.id,
                    run.definition_id,
                    run_state_str(run.state)
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
        };

        let structured = serde_json::to_value(&out).expect("WorkflowOutput serialization");
        let mut result = CallToolResult::success(vec![Content::text(text)]);
        result.structured_content = Some(structured);
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// Output mappers
// ---------------------------------------------------------------------------

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
    }
}

fn step_to_output(s: &WorkflowStepDef) -> WorkflowStepOutput {
    match s {
        WorkflowStepDef::AgentStep {
            name,
            skill,
            sandbox,
            timeout_seconds,
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

// ---------------------------------------------------------------------------
// Text formatting helpers (human-readable companion to structured_content)
// ---------------------------------------------------------------------------

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
    out.push_str(&format!("steps:       {}\n", d.steps.len()));
    for step in &d.steps {
        match step {
            WorkflowStepDef::AgentStep {
                name,
                skill,
                sandbox,
                timeout_seconds,
            } => {
                out.push_str(&format!(
                    "  - agent_step name={name} skill={skill} sandbox={sandbox:?} timeout_s={timeout_seconds:?}\n"
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
        let output = r
            .step_results
            .last()
            .and_then(|sr| {
                if let Some(err) = &sr.error {
                    Some(format!("ERROR: {err}"))
                } else {
                    sr.output.as_ref().map(|v| match v {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                }
            })
            .unwrap_or_else(|| "—".to_string());
        lines.push(format!(
            "{:<38} {:<12} {:<28} {}",
            r.id,
            run_state_str(r.state),
            started,
            truncate(&output.replace('\n', " "), 60)
        ));
    }
    lines.join("\n")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}
