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

/// A single search result for the web UI.
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

#[allow(dead_code)]
pub struct AuditEntryView {
    pub acting_user: Option<String>,
    pub workspace: Option<String>,
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

// ── Workspaces ───────────────────────────────────────────────────────

pub struct WorkspaceView {
    pub id: String,
    pub name: String,
    pub description: String,
    pub created_at: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "workspace_new.html")]
pub struct WorkspaceNewTemplate {}

#[derive(Template, WebTemplate)]
#[template(path = "workspace_detail.html")]
pub struct WorkspaceDetailTemplate {
    pub workspace: WorkspaceView,
    pub lead_agent: Option<AgentView>,
    pub agents: Vec<AgentView>,
}

// ── Skills ────────────────────────────────────────────────────────────

#[allow(dead_code)]
pub struct SkillView {
    pub id: String,
    pub workspace_id: String,
    pub name: String,
    pub description: String,
    pub body: String,
    pub created_at: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "workspace_skills.html")]
pub struct WorkspaceSkillsPageTemplate {
    pub workspace: WorkspaceView,
    pub lead_agent: Option<AgentView>,
    pub agents: Vec<AgentView>,
    pub skills: Vec<SkillView>,
}

#[derive(Template, WebTemplate)]
#[template(path = "workspace_skill_detail.html")]
pub struct WorkspaceSkillDetailTemplate {
    pub workspace: WorkspaceView,
    pub lead_agent: Option<AgentView>,
    pub agents: Vec<AgentView>,
    pub skill: SkillView,
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
    pub workspace_id: String,
    pub name: String,
    pub state: String,
    /// Reason for the most recent provisioning failure. Set when the
    /// sandbox is in the `errored` state; rendered as a banner on the
    /// detail page.
    pub last_error: Option<String>,
    pub mode_label: String,
    pub repo_url: Option<String>,
    pub branch: Option<String>,
    pub cpu: String,
    pub memory: String,
    pub disk_size: String,
    pub created_at: String,
    /// Short one-liner for the list view: `"—"`, `"system prompt"`,
    /// `"3 skills"`, or `"system prompt + 3 skills"`.
    pub exports_summary: String,
    pub exported_system_prompt: Option<ExportedFileView>,
    pub exported_skills: Vec<ExportedSkillView>,
}

#[derive(Template, WebTemplate)]
#[template(path = "workspace_sandboxes.html")]
pub struct WorkspaceSandboxesPageTemplate {
    pub workspace: WorkspaceView,
    pub lead_agent: Option<AgentView>,
    pub agents: Vec<AgentView>,
    pub sandboxes: Vec<SandboxView>,
}

#[derive(Template, WebTemplate)]
#[template(path = "workspace_sandbox_new.html")]
pub struct WorkspaceSandboxNewTemplate {
    pub workspace: WorkspaceView,
    pub lead_agent: Option<AgentView>,
    pub agents: Vec<AgentView>,
}

#[derive(Template, WebTemplate)]
#[template(path = "workspace_sandbox_detail.html")]
pub struct WorkspaceSandboxDetailTemplate {
    pub workspace: WorkspaceView,
    pub lead_agent: Option<AgentView>,
    pub agents: Vec<AgentView>,
    pub sandbox: SandboxView,
}

// ── Agents (workspace-context create form) ─────────────────────────────

/// Minimal dropdown option for the sandbox attachment selector on the
/// new-agent form. Only sandboxes that exist in the workspace are offered.
#[allow(dead_code)]
pub struct SandboxOptionView {
    pub id: String,
    pub name: String,
    pub state: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "workspace_agent_new.html")]
pub struct WorkspaceAgentNewTemplate {
    pub workspace: WorkspaceView,
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
    pub workspace_id: String,
    pub name: String,
    pub role: String,
    /// True when the agent is the workspace lead. The lead never runs in a
    /// sandbox, so the attach form is hidden in the detail view.
    pub is_lead: bool,
    pub attached_sandbox: Option<AttachedSandboxView>,
}

#[derive(Template, WebTemplate)]
#[template(path = "workspace_agent_detail.html")]
pub struct WorkspaceAgentDetailTemplate {
    pub workspace: WorkspaceView,
    pub lead_agent: Option<AgentView>,
    pub agents: Vec<AgentView>,
    pub agent: AgentDetailView,
    /// Used to populate the attach-sandbox dropdown when the agent has no
    /// current attachment. Empty when `agent.attached_sandbox` is `Some`.
    pub sandbox_options: Vec<SandboxOptionView>,
    /// Flash message surfaced after a failed attach/detach (arrives via
    /// `?error=...` on the redirect back to this page).
    pub error: Option<String>,
}

// ── Workspace Secrets ────────────────────────────────────────────────

pub struct WorkspaceSecretView {
    pub id: String,
    pub workspace_id: String,
    pub name: String,
    pub secret_type: String,
    pub updated_at: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "workspace_secrets_list.html")]
pub struct WorkspaceSecretsListTemplate {
    pub secrets: Vec<WorkspaceSecretView>,
}

#[derive(Template, WebTemplate)]
#[template(path = "workspace_sidebar_list.html")]
pub struct WorkspaceSidebarListTemplate {
    pub workspaces: Vec<WorkspaceView>,
}

pub struct AgentView {
    pub id: String,
    pub name: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "workspace_hub.html")]
pub struct WorkspaceHubTemplate {
    pub workspaces: Vec<WorkspaceView>,
    pub selected_workspace: Option<WorkspaceView>,
    /// Flat ID string for dropdown comparison (avoids Askama ref issues).
    pub selected_workspace_id: String,
    pub lead_agent: Option<AgentView>,
    pub agents: Vec<AgentView>,
    pub selected_agent_id: String,
    /// The agent the chat view is rendering for. Used to show that
    /// agent's name in the chat header so it updates when the user
    /// switches between agents (instead of showing the workspace name).
    pub selected_agent: Option<AgentView>,
}

#[derive(Template, WebTemplate)]
#[template(path = "workspace_chat.html")]
pub struct WorkspaceChatTemplate {
    pub workspace: WorkspaceView,
    pub agent_id: String,
}

// ── CLI Login ─────────────────────────────────────────────────────────

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
