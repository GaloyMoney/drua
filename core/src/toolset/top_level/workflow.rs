//! `workflow` — consolidated workspace-scoped workflow management.
//!
//! Single tool with a `command` discriminator (mirrors `agent`/`sandbox`):
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
    WorkflowDefinition, WorkflowRun, WorkflowStepDef, WorkflowTrigger, Workflows,
};

use super::super::error::ToolSetsError;
use super::super::traits::TopLevelTool;
use super::{parse_params, schema_for};

// ---------------------------------------------------------------------------
// Params
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum WorkflowCommand {
    /// Create a new workflow definition (MVP: single-step webhook workflows).
    Create,
    /// List workflow definitions in the workspace.
    List,
    /// Get a workflow definition by ID.
    Get,
    /// Manually trigger a workflow run.
    Trigger,
    /// List runs for a workflow definition.
    Runs,
}

impl WorkflowCommand {
    fn audit_action(&self) -> &'static str {
        match self {
            Self::Create => "workflow.create",
            Self::List => "workflow.list",
            Self::Get => "workflow.get",
            Self::Trigger => "workflow.trigger",
            Self::Runs => "workflow.runs",
        }
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
struct WorkflowParams {
    /// Which workflow operation to perform.
    command: WorkflowCommand,

    // -- create fields --
    /// Workflow display name (required for `create`).
    name: Option<String>,
    /// Optional human-readable description.
    description: Option<String>,
    /// Webhook provider tag (e.g. "honeycomb"). When omitted defaults to a
    /// generic `Authorization: Bearer <secret>` shared-token webhook. If
    /// you want a `Manual` trigger, pass `manual: true` instead.
    provider: Option<String>,
    /// Set to `true` to create a manually-triggered workflow (no webhook).
    #[serde(default)]
    manual: bool,
    /// Skill name for the single agent step (required for `create`).
    skill: Option<String>,
    /// Optional sandbox name to attach to the agent step.
    sandbox: Option<String>,
    /// Optional per-step timeout in seconds.
    timeout_seconds: Option<u64>,

    // -- get / trigger / runs fields --
    /// ID of the workflow definition (required for `get`, `trigger`, `runs`).
    #[schemars(with = "Option<uuid::Uuid>")]
    definition_id: Option<WorkflowDefinitionId>,

    // -- trigger field --
    /// Optional JSON payload to use as the trigger context (defaults to
    /// `{}`). Only used by `trigger`.
    payload: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Tool
// ---------------------------------------------------------------------------

pub struct WorkflowTool {
    workflows: Arc<Workflows>,
    /// Optional public-facing host (e.g. `https://drua.example.com`) used
    /// to render the webhook URL on `create` responses. When `None` the
    /// response shows just the path.
    public_host: Option<String>,
}

impl WorkflowTool {
    pub fn new(workflows: Arc<Workflows>, public_host: Option<String>) -> Self {
        Self {
            workflows,
            public_host,
        }
    }
}

static SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<WorkflowParams>);

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
        let params: WorkflowParams = parse_params(arguments)?;

        Audit::record_action(params.command.audit_action());

        match params.command {
            WorkflowCommand::Create => {
                if !subject.can_write_workspace() {
                    return Err(ToolSetsError::Unauthorized);
                }
                let name = params.name.ok_or_else(|| {
                    ToolSetsError::MissingArgument("name is required for create".into())
                })?;
                let skill = params.skill.ok_or_else(|| {
                    ToolSetsError::MissingArgument("skill is required for create".into())
                })?;

                let trigger = if params.manual {
                    WorkflowTrigger::Manual
                } else {
                    // Empty secret → Workflows::create generates one.
                    WorkflowTrigger::Webhook {
                        provider: params.provider.clone(),
                        secret: String::new(),
                    }
                };

                let step = WorkflowStepDef::AgentStep {
                    name: "step".into(),
                    skill,
                    sandbox: params.sandbox,
                    timeout_seconds: params.timeout_seconds,
                };

                let definition = self
                    .workflows
                    .create(
                        subject,
                        workspace_id,
                        name,
                        params.description,
                        trigger,
                        vec![step],
                    )
                    .await
                    .map_err(|e| ToolSetsError::Workflow(e.to_string()))?;

                Ok(CallToolResult::success(vec![Content::text(
                    self.format_create(&definition),
                )]))
            }

            WorkflowCommand::List => {
                let definitions = self
                    .workflows
                    .list_for_workspace(subject, workspace_id)
                    .await
                    .map_err(|e| ToolSetsError::Workflow(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(format_list(
                    &definitions,
                ))]))
            }

            WorkflowCommand::Get => {
                let definition_id = params.definition_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("definition_id is required for get".into())
                })?;
                let definition = self
                    .workflows
                    .find_by_id(subject, definition_id)
                    .await
                    .map_err(|e| ToolSetsError::Workflow(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(format_get(
                    &definition,
                ))]))
            }

            WorkflowCommand::Trigger => {
                if !subject.can_write_workspace() {
                    return Err(ToolSetsError::Unauthorized);
                }
                let definition_id = params.definition_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("definition_id is required for trigger".into())
                })?;
                let payload = params
                    .payload
                    .unwrap_or_else(|| serde_json::Value::Object(Default::default()));
                let run = self
                    .workflows
                    .trigger_run(subject, definition_id, payload)
                    .await
                    .map_err(|e| ToolSetsError::Workflow(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "Workflow run started.\n  run_id:        {}\n  definition_id: {}\n  state:         {:?}",
                    run.id, run.definition_id, run.state
                ))]))
            }

            WorkflowCommand::Runs => {
                let definition_id = params.definition_id.ok_or_else(|| {
                    ToolSetsError::MissingArgument("definition_id is required for runs".into())
                })?;
                let runs = self
                    .workflows
                    .list_runs(subject, definition_id)
                    .await
                    .map_err(|e| ToolSetsError::Workflow(e.to_string()))?;
                Ok(CallToolResult::success(vec![Content::text(format_runs(
                    &runs,
                ))]))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

impl WorkflowTool {
    fn format_create(&self, def: &WorkflowDefinition) -> String {
        let (trigger_label, secret) = match &def.trigger {
            WorkflowTrigger::Manual => ("manual".to_string(), None),
            WorkflowTrigger::Webhook { provider, secret } => {
                let label = match provider {
                    Some(p) => format!("webhook ({p})"),
                    None => "webhook".to_string(),
                };
                (label, Some(secret.clone()))
            }
        };
        let webhook_url = secret.as_ref().map(|_| match &self.public_host {
            Some(host) => format!("{host}/webhooks/{}", def.id),
            None => format!(
                "/webhooks/{} (set public host in config to render full URL)",
                def.id
            ),
        });

        let mut out = String::new();
        out.push_str("Workflow created.\n");
        out.push_str(&format!("  id:             {}\n", def.id));
        out.push_str(&format!("  name:           {}\n", def.name));
        out.push_str(&format!("  trigger:        {trigger_label}\n"));
        if let Some(url) = webhook_url {
            out.push_str(&format!("  webhook_url:    {url}\n"));
        }
        if let Some(s) = secret {
            out.push_str(&format!("  webhook_secret: {s}\n"));
        }
        out.push_str("  status:         enabled\n");
        if matches!(def.trigger, WorkflowTrigger::Webhook { provider: Some(ref p), .. } if p == "honeycomb")
        {
            out.push_str(
                "\nConfigure your Honeycomb webhook recipient with the URL and secret above.\n",
            );
        }
        out
    }
}

fn format_list(defs: &[WorkflowDefinition]) -> String {
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

fn format_get(d: &WorkflowDefinition) -> String {
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

fn format_runs(runs: &[WorkflowRun]) -> String {
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
            format!("{:?}", r.state).to_lowercase(),
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
