use askama::Template;
use askama_web::WebTemplate;

use galoy_agents_core::code_assistant::logs::{CodeAssistantRequestRow, DashboardStats};
use galoy_agents_core::code_assistant::LabelOriginCounts;

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
pub struct AuditSubjectView {
    pub label: String,
    pub owner: Option<String>,
}

#[allow(dead_code)]
pub struct AuditEntryView {
    pub subject: AuditSubjectView,
    pub action: String,
    pub outcome: String,
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

// ── Reports ──────────────────────────────────────────────────────────

#[derive(Template, WebTemplate)]
#[template(path = "reports.html")]
pub struct ReportsTemplate {
    pub enabled: bool,
}

pub struct ReportView {
    pub id: String,
    pub short_id: String,
    pub title: String,
    pub content: String,
    pub tags: String,
    pub created_at: String,
    pub pinned: bool,
}

pub struct ReportSearchResultView {
    pub id: String,
    pub short_id: String,
    pub title: String,
    pub content: String,
    pub tags: String,
    pub score: String,
    pub pinned: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "reports_list.html")]
pub struct ReportsListTemplate {
    pub reports: Vec<ReportView>,
}

#[derive(Template, WebTemplate)]
#[template(path = "reports_search_results.html")]
pub struct ReportsSearchResultsTemplate {
    pub query: String,
    pub results: Vec<ReportSearchResultView>,
    pub error: Option<String>,
}

#[derive(Template, WebTemplate)]
#[template(path = "reports_detail.html")]
pub struct ReportDetailTemplate {
    pub report: ReportView,
}

// ── Workspaces ───────────────────────────────────────────────────────

pub struct WorkspaceView {
    pub id: String,
    pub name: String,
    pub description: String,
    pub created_at: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "workspaces.html")]
pub struct WorkspacesTemplate {}

#[derive(Template, WebTemplate)]
#[template(path = "workspace_list.html")]
pub struct WorkspaceListTemplate {
    pub workspaces: Vec<WorkspaceView>,
}

#[derive(Template, WebTemplate)]
#[template(path = "workspace_new.html")]
pub struct WorkspaceNewTemplate {}

#[derive(Template, WebTemplate)]
#[template(path = "workspace_detail.html")]
pub struct WorkspaceDetailTemplate {
    pub workspace: WorkspaceView,
    pub agent_id: String,
}

// ── Projects ────────────────────────────────────────────────────��────

pub struct ProjectView {
    pub id: String,
    pub workspace_id: String,
    pub name: String,
    pub description: String,
    pub status: String,
    pub agent_count: usize,
    pub created_at: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "project_list.html")]
pub struct ProjectListTemplate {
    pub projects: Vec<ProjectView>,
}

pub struct ProjectAgentView {
    pub id: String,
    pub name: String,
    pub agent_type: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "project_detail.html")]
pub struct ProjectDetailTemplate {
    pub project: ProjectView,
    pub unassigned_agents: Vec<ProjectAgentView>,
}

#[derive(Template, WebTemplate)]
#[template(path = "project_agents_list.html")]
pub struct ProjectAgentsListTemplate {
    pub agents: Vec<ProjectAgentView>,
    pub workspace_id: String,
    pub project_id: String,
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
#[template(path = "workspace_chat.html")]
pub struct WorkspaceChatTemplate {
    pub workspace: WorkspaceView,
    pub agent_id: String,
}

pub struct AgentConfigView {
    pub model: String,
    pub max_tokens: u32,
    pub max_turns: u32,
    pub resource_cpu: String,
    pub resource_mem: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "agent_config_panel.html")]
pub struct AgentConfigPanelTemplate {
    pub agent_id: String,
    pub config: AgentConfigView,
    pub saved: bool,
}
