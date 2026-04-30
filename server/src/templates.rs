use askama::Template;
use askama_web::WebTemplate;

use drua_core::code_assistant::logs::{CodeAssistantRequestRow, DashboardStats};
use drua_core::code_assistant::LabelOriginCounts;

pub struct McpCredsView {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub is_revoked: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "login.html")]
pub struct LoginTemplate {
    pub error: Option<String>,
    pub dev_auth: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "dashboard.html")]
pub struct DashboardTemplate {
    pub user_name: String,
    pub mcp_creds: Vec<McpCredsView>,
}

#[derive(Template, WebTemplate)]
#[template(path = "mcp_creds_created.html")]
pub struct McpCredsCreatedTemplate {
    pub creds_name: String,
    pub mcp_json: String,
    pub cli_command: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "mcp_creds_row.html")]
pub struct McpCredsRowTemplate {
    pub creds: McpCredsView,
}

#[derive(Template, WebTemplate)]
#[template(path = "mcp_creds_list.html")]
pub struct McpCredsListTemplate {
    pub mcp_creds: Vec<McpCredsView>,
}

#[derive(Template, WebTemplate)]
#[template(path = "code_assistant.html")]
pub struct CodeAssistantTemplate {
    pub stats: DashboardStats,
    pub label_origins: LabelOriginCounts,
}

#[derive(Template, WebTemplate)]
#[template(path = "code_assistant_disabled.html")]
pub struct CodeAssistantDisabledTemplate {}

#[derive(Template, WebTemplate)]
#[template(path = "code_assistant_recent.html")]
pub struct CodeAssistantRecentTemplate {
    pub rows: Vec<CodeAssistantRequestRow>,
}

pub struct SearchResultView {
    pub file_path: String,
    pub repo: String,
    pub score: String,
    pub labels: String,
    pub lines: String,
    pub content: String,
    pub language: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "code_assistant_search_results.html")]
pub struct CodeAssistantSearchResultsTemplate {
    pub query: String,
    pub results: Vec<SearchResultView>,
    pub error: Option<String>,
}

#[derive(Template, WebTemplate)]
#[template(path = "audit.html")]
pub struct AuditTemplate {}

pub struct LibraryProjectOption {
    pub id: String,
    pub name: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "library.html")]
pub struct LibraryTemplate {
    pub projects: Vec<LibraryProjectOption>,
}

#[allow(dead_code)]
pub struct LibraryHitView {
    pub id: String,
    /// Already-formatted scope line — e.g. `project: oncall` for
    /// project-scoped hits, `space: my-runbooks` for space files,
    /// or `global` for global skills. The template renders this
    /// verbatim.
    pub scope_label: String,
    pub type_label: String,
    /// CSS class on the type badge: `skill`, `note`, `workflow`, or
    /// `space_file`.
    pub type_class: String,
    pub title: String,
    pub snippet: String,
    pub score: String,
    pub tags: Vec<String>,
    /// `Some` when a project-scoped detail page exists for this type.
    pub detail_url: Option<String>,
}

#[derive(Template, WebTemplate)]
#[template(path = "library_search_results.html")]
pub struct LibrarySearchResultsTemplate {
    pub query: String,
    pub hits: Vec<LibraryHitView>,
    pub error: Option<String>,
}

#[allow(dead_code)]
pub struct AuditEntryView {
    pub acting_user: Option<String>,
    pub project: Option<String>,
    pub acting_agent: Option<String>,
    pub on_behalf_of: Option<String>,
    pub entrypoint: Option<String>,
    pub action: String,
    pub outcome: String,
    pub error: bool,
    pub duration_ms: Option<i64>,
    pub tokens_returned: Option<i64>,
    pub metadata: serde_json::Value,
    pub recorded_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Template, WebTemplate)]
#[template(path = "audit_entries.html")]
pub struct AuditEntriesTemplate {
    pub entries: Vec<AuditEntryView>,
}

pub struct ProjectView {
    pub id: String,
    pub name: String,
    pub description: String,
    pub created_at: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "project_new.html")]
pub struct ProjectNewTemplate {}

#[derive(Template, WebTemplate)]
#[template(path = "project_detail.html")]
pub struct ProjectDetailTemplate {
    pub project: ProjectView,
    pub lead_agent: Option<AgentView>,
    pub agents: Vec<AgentView>,
}

#[allow(dead_code)]
pub struct SkillView {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub description: String,
    pub body: String,
    pub created_at: String,
    pub is_global: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "project_skills.html")]
pub struct ProjectSkillsPageTemplate {
    pub project: ProjectView,
    pub lead_agent: Option<AgentView>,
    pub agents: Vec<AgentView>,
    pub skills: Vec<SkillView>,
    pub library_repo_url: Option<String>,
}

#[derive(Template, WebTemplate)]
#[template(path = "project_skill_detail.html")]
pub struct ProjectSkillDetailTemplate {
    pub project: ProjectView,
    pub lead_agent: Option<AgentView>,
    pub agents: Vec<AgentView>,
    pub skill: SkillView,
}

// ── Workflows ─────────────────────────────────────────────────────────

#[allow(dead_code)]
pub struct WorkflowDefinitionView {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// `"manual"` or `"webhook"`.
    pub trigger_type: String,
    pub trigger_provider: Option<String>,
    /// Detail page only — never set on listings.
    pub webhook_secret: Option<String>,
    pub webhook_url: Option<String>,
    pub steps: Vec<WorkflowStepView>,
    pub sandboxes: Vec<WorkflowSandboxView>,
    pub created_at: String,
}

#[allow(dead_code)]
pub struct WorkflowStepView {
    pub name: String,
    pub step_type: String,
    pub skill: String,
    pub sandbox: Option<String>,
    pub timeout_seconds: Option<u64>,
}

#[allow(dead_code)]
pub struct WorkflowSandboxView {
    pub name: String,
    /// `"scratch"`, `"repo"`, or `"preexisting"`.
    pub kind: String,
    pub repo_url: Option<String>,
    pub branch: Option<String>,
}

#[allow(dead_code)]
pub struct WorkflowRunView {
    pub id: String,
    pub definition_id: String,
    /// `pending` / `running` / `succeeded` / `failed`.
    pub state: String,
    pub started_at: String,
    pub completed_at: Option<String>,
    /// Last step's text output (or `ERROR: …`) — for the listing.
    pub output_summary: String,
}

#[allow(dead_code)]
pub struct StepResultView {
    pub name: String,
    pub state: String,
    pub output: Option<String>,
    pub error: Option<String>,
    pub completed_at: Option<String>,
}

#[derive(Template, WebTemplate)]
#[template(path = "project_workflows.html")]
pub struct ProjectWorkflowsPageTemplate {
    pub project: ProjectView,
    pub lead_agent: Option<AgentView>,
    pub agents: Vec<AgentView>,
    pub workflows: Vec<WorkflowDefinitionView>,
}

#[derive(Template, WebTemplate)]
#[template(path = "project_workflow_detail.html")]
pub struct ProjectWorkflowDetailTemplate {
    pub project: ProjectView,
    pub lead_agent: Option<AgentView>,
    pub agents: Vec<AgentView>,
    pub workflow: WorkflowDefinitionView,
    pub recent_runs: Vec<WorkflowRunView>,
    pub flash: Option<String>,
}

#[derive(Template, WebTemplate)]
#[template(path = "project_workflow_run_detail.html")]
pub struct ProjectWorkflowRunDetailTemplate {
    pub project: ProjectView,
    pub lead_agent: Option<AgentView>,
    pub agents: Vec<AgentView>,
    pub workflow_id: String,
    pub workflow_name: String,
    pub run: WorkflowRunView,
    pub steps: Vec<StepResultView>,
    pub run_agents: Vec<AgentView>,
}

// ── Sandboxes ─────────────────────────────────────────────────────────

#[allow(dead_code)]
pub struct ExportedFileView {
    pub file_name: String,
    pub content: String,
}

#[allow(dead_code)]
pub struct ExportedSkillView {
    pub name: String,
    pub content: String,
    pub description: Option<String>,
}

#[allow(dead_code)]
pub struct SandboxView {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub state: String,
    /// Set when the sandbox is in the `errored` state.
    pub last_error: Option<String>,
    pub mode_label: String,
    pub repo_url: Option<String>,
    pub branch: Option<String>,
    pub cpu: String,
    pub memory: String,
    pub disk_size: String,
    pub created_at: String,
    /// E.g. `"—"`, `"system prompt"`, `"3 skills"`, `"system prompt + 3 skills"`.
    pub exports_summary: String,
    pub exported_system_prompt: Option<ExportedFileView>,
    pub exported_skills: Vec<ExportedSkillView>,
}

#[derive(Template, WebTemplate)]
#[template(path = "project_sandboxes.html")]
pub struct ProjectSandboxesPageTemplate {
    pub project: ProjectView,
    pub lead_agent: Option<AgentView>,
    pub agents: Vec<AgentView>,
    pub sandboxes: Vec<SandboxView>,
}

#[derive(Template, WebTemplate)]
#[template(path = "project_sandbox_new.html")]
pub struct ProjectSandboxNewTemplate {
    pub project: ProjectView,
    pub lead_agent: Option<AgentView>,
    pub agents: Vec<AgentView>,
}

#[derive(Template, WebTemplate)]
#[template(path = "project_sandbox_detail.html")]
pub struct ProjectSandboxDetailTemplate {
    pub project: ProjectView,
    pub lead_agent: Option<AgentView>,
    pub agents: Vec<AgentView>,
    pub sandbox: SandboxView,
}

#[allow(dead_code)]
pub struct SandboxOptionView {
    pub id: String,
    pub name: String,
    pub state: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "project_agent_new.html")]
pub struct ProjectAgentNewTemplate {
    pub project: ProjectView,
    pub lead_agent: Option<AgentView>,
    pub agents: Vec<AgentView>,
    pub sandbox_options: Vec<SandboxOptionView>,
}

#[allow(dead_code)]
pub struct AttachedSandboxView {
    pub id: String,
    pub name: String,
    pub mode: String,
    pub state: String,
}

#[allow(dead_code)]
pub struct AgentDetailView {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub role: String,
    /// Lead never runs in a sandbox, so the attach form is hidden.
    pub is_lead: bool,
    pub attached_sandbox: Option<AttachedSandboxView>,
}

#[derive(Template, WebTemplate)]
#[template(path = "project_agent_detail.html")]
pub struct ProjectAgentDetailTemplate {
    pub project: ProjectView,
    pub lead_agent: Option<AgentView>,
    pub agents: Vec<AgentView>,
    pub agent: AgentDetailView,
    /// Empty when `agent.attached_sandbox` is `Some`.
    pub sandbox_options: Vec<SandboxOptionView>,
    /// Flash message after a failed attach/detach (`?error=...` on redirect).
    pub error: Option<String>,
}

#[allow(dead_code)]
pub struct ChatHistoryBlockView {
    /// `text`, `tool_use`, `thinking`, `tool_result`, `sandbox_notification`.
    pub kind: String,
    pub text: Option<String>,
    pub tool_name: Option<String>,
    /// Pretty-printed JSON for `tool_use.input`.
    pub tool_input: Option<String>,
    pub is_error: Option<bool>,
}

#[allow(dead_code)]
pub struct ChatHistoryMessageView {
    pub sequence: usize,
    /// `user` or `assistant`.
    pub role: String,
    pub blocks: Vec<ChatHistoryBlockView>,
}

#[derive(Template, WebTemplate)]
#[template(path = "project_agent_history.html")]
pub struct ProjectAgentHistoryTemplate {
    pub project: ProjectView,
    pub lead_agent: Option<AgentView>,
    pub agents: Vec<AgentView>,
    pub agent: AgentDetailView,
    pub messages: Vec<ChatHistoryMessageView>,
    /// `Some((workflow_id, run_id))` for workflow-spawned agents — used
    /// to render a back link to the run detail page.
    pub workflow_run: Option<(String, String)>,
}

pub struct ProjectSecretView {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub secret_type: String,
    pub updated_at: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "project_secrets_list.html")]
pub struct ProjectSecretsListTemplate {
    pub secrets: Vec<ProjectSecretView>,
}

#[derive(Template, WebTemplate)]
#[template(path = "project_sidebar_list.html")]
pub struct ProjectSidebarListTemplate {
    pub projects: Vec<ProjectView>,
}

pub struct AgentView {
    pub id: String,
    pub name: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "project_hub.html")]
pub struct ProjectHubTemplate {
    pub projects: Vec<ProjectView>,
    pub selected_project: Option<ProjectView>,
    /// Flat ID string for dropdown comparison (avoids Askama ref issues).
    pub selected_project_id: String,
    pub lead_agent: Option<AgentView>,
    pub agents: Vec<AgentView>,
    pub selected_agent_id: String,
    /// The agent the chat view is rendering for; drives the chat header name.
    pub selected_agent: Option<AgentView>,
    /// Configured LLM model for the selected agent's role; rendered next to
    /// "Agent chat" so the user can confirm what's actually serving prompts.
    pub selected_agent_model: Option<String>,
}

#[derive(Template, WebTemplate)]
#[template(path = "project_chat.html")]
pub struct ProjectChatTemplate {
    pub project: ProjectView,
    pub agent_id: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "cli_login.html")]
pub struct CliLoginTemplate {
    pub dev_auth: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "cli_token.html")]
pub struct CliTokenTemplate {
    pub token: String,
}
