use std::sync::Arc;

use async_graphql::{ComplexObject, Context, Enum, InputObject, SimpleObject};

use super::agent::{Agent, ModelChain, SandboxAttachmentMode};
use super::primitives::*;

use drua_core::sandbox::{SandboxMode, SandboxSpecs};
use drua_core::workflow::{
    StepResult as DomainStepResult, WorkflowDefinition as DomainWorkflowDefinition,
    WorkflowRun as DomainWorkflowRun, WorkflowRunState as DomainWorkflowRunState,
    WorkflowSandboxDecl as DomainWorkflowSandboxDecl, WorkflowStepDef as DomainWorkflowStepDef,
    WorkflowStepState as DomainWorkflowStepState, WorkflowTrigger as DomainWorkflowTrigger,
};

#[derive(InputObject)]
pub struct WorkflowDeleteInput {
    pub id: WorkflowDefinitionId,
}

#[derive(SimpleObject)]
pub struct WorkflowDeletePayload {
    pub deleted_id: WorkflowDefinitionId,
}

#[derive(SimpleObject, Clone)]
#[graphql(complex)]
pub struct WorkflowDefinition {
    id: WorkflowDefinitionId,
    project_id: ProjectId,
    name: String,
    description: Option<String>,
    trigger: WorkflowTrigger,
    steps: Vec<WorkflowStep>,
    sandboxes: Vec<WorkflowSandbox>,
    model_chain: Option<ModelChain>,
    /// `true` when the workflow is paused — no runs fire (cron fires
    /// are skipped, webhook deliveries suppressed, manual triggers
    /// error) until resumed.
    paused: bool,
    created_at: Timestamp,
    updated_at: Timestamp,
    yaml: String,

    #[graphql(skip)]
    pub(super) entity: Arc<DomainWorkflowDefinition>,
}

#[ComplexObject]
impl WorkflowDefinition {
    async fn recent_runs(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 10)] first: i32,
    ) -> async_graphql::Result<Vec<WorkflowRun>> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let runs = app.workflows().list_runs(sub, self.entity.id).await?;
        Ok(limit(runs, first)
            .into_iter()
            .map(WorkflowRun::from)
            .collect())
    }
}

impl From<DomainWorkflowDefinition> for WorkflowDefinition {
    fn from(entity: DomainWorkflowDefinition) -> Self {
        let created_at = entity.created_at();
        let updated_at = entity.updated_at();
        let mut trigger = WorkflowTrigger::from(&entity.trigger);
        if matches!(entity.trigger, DomainWorkflowTrigger::Webhook { .. }) {
            trigger.webhook_url = Some(format!("/webhooks/{}", entity.id));
        }
        trigger.next_run_at = entity.next_run_at(chrono::Utc::now()).map(Into::into);
        let yaml = entity.canonical_yaml();
        Self {
            id: entity.id,
            project_id: entity.project_id,
            name: entity.name.clone(),
            description: entity.description.clone(),
            trigger,
            steps: entity.steps.iter().map(WorkflowStep::from).collect(),
            sandboxes: entity.sandboxes.iter().map(WorkflowSandbox::from).collect(),
            model_chain: entity.model_chain.clone().map(Into::into),
            paused: entity.paused,
            created_at: created_at.into(),
            updated_at: updated_at.into(),
            yaml,
            entity: Arc::new(entity),
        }
    }
}

#[derive(SimpleObject, Clone)]
pub struct WorkflowTrigger {
    kind: WorkflowTriggerKind,
    provider: Option<String>,
    cron_schedule: Option<String>,
    cron_timezone: Option<String>,
    next_run_at: Option<Timestamp>,
    condition: Option<String>,
    webhook_url: Option<String>,
}

impl From<&DomainWorkflowTrigger> for WorkflowTrigger {
    fn from(trigger: &DomainWorkflowTrigger) -> Self {
        match trigger {
            DomainWorkflowTrigger::Manual { condition } => Self {
                kind: WorkflowTriggerKind::Manual,
                provider: None,
                cron_schedule: None,
                cron_timezone: None,
                next_run_at: None,
                condition: condition.clone(),
                webhook_url: None,
            },
            DomainWorkflowTrigger::Webhook {
                provider,
                condition,
                ..
            } => Self {
                kind: WorkflowTriggerKind::Webhook,
                provider: provider.clone(),
                cron_schedule: None,
                cron_timezone: None,
                next_run_at: None,
                condition: condition.clone(),
                webhook_url: None,
            },
            DomainWorkflowTrigger::Cron {
                schedule,
                timezone,
                condition,
            } => Self {
                kind: WorkflowTriggerKind::Cron,
                provider: None,
                cron_schedule: Some(schedule.clone()),
                cron_timezone: timezone.clone(),
                // Filled by the definition conversion, which knows the
                // pause state (`WorkflowDefinition::next_run_at`).
                next_run_at: None,
                condition: condition.clone(),
                webhook_url: None,
            },
        }
    }
}

#[derive(Enum, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowTriggerKind {
    Manual,
    Webhook,
    Cron,
}

#[derive(SimpleObject, Clone)]
pub struct WorkflowStep {
    name: String,
    step_type: WorkflowStepType,
    skill: Option<String>,
    tool: Option<String>,
    sandbox: Option<String>,
    sandbox_mode: Option<SandboxAttachmentMode>,
    timeout_seconds: Option<i32>,
    condition: Option<String>,
    output_schema: Option<JsonValue>,
    /// Wait-step `outputs` map serialized as `{ name: cel_expr }`.
    outputs: Option<JsonValue>,
    /// Wait-step only: webhook provider this step parks on.
    provider: Option<String>,
    /// Wait-step only: CEL boolean evaluated against the inbound
    /// webhook payload (`resume_payload.*`) plus `trigger` and `steps`.
    resume_condition: Option<String>,
}

impl From<&DomainWorkflowStepDef> for WorkflowStep {
    fn from(step: &DomainWorkflowStepDef) -> Self {
        match step {
            DomainWorkflowStepDef::AgentStep {
                name,
                skill,
                sandbox,
                sandbox_mode,
                timeout_seconds,
                output_schema,
                condition,
                ..
            } => Self {
                name: name.clone(),
                step_type: WorkflowStepType::AgentStep,
                skill: Some(skill.clone()),
                tool: None,
                sandbox: sandbox.clone(),
                sandbox_mode: sandbox_mode.map(SandboxAttachmentMode::from),
                timeout_seconds: timeout_seconds.map(|s| s.min(i32::MAX as u64) as i32),
                condition: condition.clone(),
                output_schema: serde_json::to_value(output_schema.root_schema())
                    .ok()
                    .map(Into::into),
                outputs: None,
                provider: None,
                resume_condition: None,
            },
            DomainWorkflowStepDef::ToolStep {
                name,
                tool,
                timeout_seconds,
                condition,
                ..
            } => Self {
                name: name.clone(),
                step_type: WorkflowStepType::ToolStep,
                skill: None,
                tool: Some(tool.clone()),
                sandbox: None,
                sandbox_mode: None,
                timeout_seconds: timeout_seconds.map(|s| s.min(i32::MAX as u64) as i32),
                condition: condition.clone(),
                output_schema: None,
                outputs: None,
                provider: None,
                resume_condition: None,
            },
            DomainWorkflowStepDef::Wait {
                name,
                provider,
                resume_condition,
                condition,
                outputs,
                ..
            } => Self {
                name: name.clone(),
                step_type: WorkflowStepType::Wait,
                skill: None,
                tool: None,
                sandbox: None,
                sandbox_mode: None,
                timeout_seconds: None,
                condition: condition.clone(),
                output_schema: None,
                outputs: serde_json::to_value(outputs).ok().map(Into::into),
                provider: Some(provider.clone()),
                resume_condition: Some(resume_condition.clone()),
            },
        }
    }
}

#[derive(Enum, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowStepType {
    AgentStep,
    ToolStep,
    Wait,
}

#[derive(SimpleObject, Clone)]
pub struct WorkflowSandbox {
    name: String,
    kind: WorkflowSandboxKind,
    repo_url: Option<String>,
    branch: Option<String>,
    cpu: Option<String>,
    memory: Option<String>,
    disk_size: Option<String>,
}

impl From<&DomainWorkflowSandboxDecl> for WorkflowSandbox {
    fn from(sandbox: &DomainWorkflowSandboxDecl) -> Self {
        match sandbox {
            DomainWorkflowSandboxDecl::Preexisting { name } => Self {
                name: name.clone(),
                kind: WorkflowSandboxKind::Preexisting,
                repo_url: None,
                branch: None,
                cpu: None,
                memory: None,
                disk_size: None,
            },
            DomainWorkflowSandboxDecl::Provisioned { name, mode, specs } => {
                let (kind, repo_url, branch) = match mode {
                    SandboxMode::Scratch => (WorkflowSandboxKind::Scratch, None, None),
                    SandboxMode::Repo { repo_url, branch } => (
                        WorkflowSandboxKind::Repo,
                        Some(repo_url.clone()),
                        branch.clone(),
                    ),
                };
                let (cpu, memory, disk_size) = specs_parts(specs.as_ref());
                Self {
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
}

fn specs_parts(specs: Option<&SandboxSpecs>) -> (Option<String>, Option<String>, Option<String>) {
    match specs {
        Some(specs) => (
            Some(specs.cpu.clone()),
            Some(specs.memory.clone()),
            Some(specs.disk_size.clone()),
        ),
        None => (None, None, None),
    }
}

#[derive(Enum, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowSandboxKind {
    Scratch,
    Repo,
    Preexisting,
}

#[derive(SimpleObject, Clone)]
#[graphql(complex)]
pub struct WorkflowRun {
    id: WorkflowRunId,
    definition_id: WorkflowDefinitionId,
    project_id: ProjectId,
    state: WorkflowRunState,
    started_at: Timestamp,
    completed_at: Option<Timestamp>,
    trigger_context: JsonValue,
    step_results: Vec<WorkflowStepResult>,
    steps_snapshot: Vec<WorkflowStep>,

    #[graphql(skip)]
    pub(super) entity: Arc<DomainWorkflowRun>,
}

#[ComplexObject]
impl WorkflowRun {
    async fn agents(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<Agent>> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let agents = app
            .agents()
            .list_for_workflow_run(sub, self.entity.project_id, self.entity.id)
            .await?;
        Ok(agents.into_iter().map(Agent::from).collect())
    }
}

impl From<DomainWorkflowRun> for WorkflowRun {
    fn from(entity: DomainWorkflowRun) -> Self {
        Self {
            id: entity.id,
            definition_id: entity.definition_id,
            project_id: entity.project_id,
            state: WorkflowRunState::from(entity.state),
            started_at: entity.started_at().into(),
            completed_at: entity.completed_at.map(Into::into),
            trigger_context: entity.trigger_context.clone().into(),
            step_results: entity
                .step_results
                .iter()
                .map(WorkflowStepResult::from)
                .collect(),
            steps_snapshot: entity
                .steps_snapshot
                .iter()
                .map(WorkflowStep::from)
                .collect(),
            entity: Arc::new(entity),
        }
    }
}

#[derive(Enum, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowRunState {
    Pending,
    Running,
    WaitingForEvent,
    Succeeded,
    Failed,
    Errored,
}

impl From<DomainWorkflowRunState> for WorkflowRunState {
    fn from(state: DomainWorkflowRunState) -> Self {
        match state {
            DomainWorkflowRunState::Pending => Self::Pending,
            DomainWorkflowRunState::Running => Self::Running,
            DomainWorkflowRunState::WaitingForEvent => Self::WaitingForEvent,
            DomainWorkflowRunState::Succeeded => Self::Succeeded,
            DomainWorkflowRunState::Failed => Self::Failed,
            DomainWorkflowRunState::Errored => Self::Errored,
        }
    }
}

#[derive(SimpleObject, Clone)]
pub struct WorkflowStepResult {
    name: String,
    output: Option<JsonValue>,
    error: Option<String>,
    completed_at: Option<Timestamp>,
    skipped: Option<String>,
    state: WorkflowStepState,
}

impl From<&DomainStepResult> for WorkflowStepResult {
    fn from(result: &DomainStepResult) -> Self {
        Self {
            name: result.name.clone(),
            output: result.output.clone().map(Into::into),
            error: result.error.clone(),
            completed_at: result.completed_at.map(Into::into),
            skipped: result.skipped.clone(),
            state: result.step_state().into(),
        }
    }
}

impl From<DomainWorkflowStepState> for WorkflowStepState {
    fn from(state: DomainWorkflowStepState) -> Self {
        match state {
            DomainWorkflowStepState::Pending => Self::Pending,
            DomainWorkflowStepState::WaitingForEvent => Self::WaitingForEvent,
            DomainWorkflowStepState::Succeeded => Self::Succeeded,
            DomainWorkflowStepState::Failed => Self::Failed,
            DomainWorkflowStepState::Errored => Self::Errored,
            DomainWorkflowStepState::Skipped => Self::Skipped,
        }
    }
}

#[derive(Enum, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowStepState {
    Pending,
    WaitingForEvent,
    Succeeded,
    Failed,
    Errored,
    Skipped,
}

#[derive(InputObject)]
pub struct WorkflowTriggerInput {
    pub definition_id: WorkflowDefinitionId,
    pub payload: Option<JsonValue>,
}

#[derive(SimpleObject)]
pub struct WorkflowTriggerPayload {
    pub run: Option<WorkflowRun>,
    pub filtered: bool,
}

#[derive(InputObject)]
pub struct WorkflowSetPausedInput {
    pub definition_id: WorkflowDefinitionId,
    pub paused: bool,
}

#[derive(SimpleObject)]
pub struct WorkflowSetPausedPayload {
    pub workflow: WorkflowDefinition,
}

pub fn limit<T>(items: Vec<T>, first: i32) -> Vec<T> {
    let limit = first.clamp(1, 100) as usize;
    items.into_iter().take(limit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step_result(output: Option<serde_json::Value>) -> DomainStepResult {
        DomainStepResult {
            name: "step".to_string(),
            output,
            error: None,
            completed_at: None,
            skipped: None,
            waiting_provider: None,
        }
    }

    #[test]
    fn step_state_failed_for_agent_reported_failure() {
        let result = step_result(Some(serde_json::json!({ "success": false })));
        let result = WorkflowStepResult::from(&result);
        assert!(matches!(result.state, WorkflowStepState::Failed));
    }

    #[test]
    fn step_state_succeeded_for_success_or_legacy_output() {
        let success = step_result(Some(serde_json::json!({ "success": true })));
        let success = WorkflowStepResult::from(&success);
        assert!(matches!(success.state, WorkflowStepState::Succeeded));

        let legacy = step_result(Some(serde_json::json!({ "value": "done" })));
        let legacy = WorkflowStepResult::from(&legacy);
        assert!(matches!(legacy.state, WorkflowStepState::Succeeded));
    }
}
