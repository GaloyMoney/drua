use serde_json::Value;

use super::SandboxInfo;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MillerFocus {
    #[default]
    Definitions,
    Runs,
    StepDetail,
}

#[derive(Debug, Clone, Default)]
pub struct WorkflowState {
    pub focus: MillerFocus,
    pub definitions: Vec<WorkflowDefinitionItem>,
    pub def_cursor: usize,
    pub selected_definition: Option<WorkflowDefinitionDetail>,
    pub runs: Vec<WorkflowRunItem>,
    pub run_cursor: usize,
    pub selected_run: Option<WorkflowRunDetail>,
    pub step_cursor: usize,
    pub expanded: bool,
    pub detail_scroll: u16,
    pub loading: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WorkflowDefinitionItem {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub trigger: WorkflowTriggerInfo,
    pub steps: Vec<WorkflowStepItem>,
    pub recent_runs: Vec<WorkflowRunItem>,
}

#[derive(Debug, Clone)]
pub struct WorkflowDefinitionDetail {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub yaml: String,
    pub trigger: WorkflowTriggerInfo,
    pub steps: Vec<WorkflowStepItem>,
    pub sandboxes: Vec<WorkflowSandboxItem>,
    pub recent_runs: Vec<WorkflowRunItem>,
}

#[derive(Debug, Clone, Default)]
pub struct WorkflowTriggerInfo {
    pub kind: String,
    pub provider: Option<String>,
    pub cron_schedule: Option<String>,
    pub cron_timezone: Option<String>,
    pub next_run_at: Option<String>,
    pub condition: Option<String>,
    pub webhook_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WorkflowStepItem {
    pub name: String,
    pub step_type: String,
    pub skill: Option<String>,
    pub tool: Option<String>,
    pub sandbox: Option<String>,
    pub sandbox_mode: Option<String>,
    pub condition: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WorkflowSandboxItem {
    pub name: String,
    pub kind: String,
    pub branch: Option<String>,
    pub repo_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WorkflowRunItem {
    pub id: String,
    pub state: String,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub step_results: Vec<WorkflowStepResultItem>,
}

#[derive(Debug, Clone)]
pub struct WorkflowRunDetail {
    pub id: String,
    pub definition_id: String,
    pub project_id: String,
    pub state: String,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub trigger_context: Value,
    pub steps_snapshot: Vec<WorkflowStepItem>,
    pub step_results: Vec<WorkflowStepResultItem>,
    pub agents: Vec<WorkflowAgentItem>,
}

impl WorkflowRunDetail {
    // Treat any non-pending/non-running state as terminal so unknown
    // future states (e.g. CANCELLED) stop polling instead of looping.
    pub fn is_terminal(&self) -> bool {
        !matches!(self.state.as_str(), "PENDING" | "RUNNING")
    }
}

#[derive(Debug, Clone)]
pub struct WorkflowStepResultItem {
    pub name: String,
    pub state: String,
    pub output: Option<Value>,
    pub error: Option<String>,
    pub skipped: Option<String>,
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WorkflowAgentItem {
    pub id: String,
    pub name: String,
    pub role: String,
    pub model: String,
    pub sandbox: Option<SandboxInfo>,
}
