use askama::Template;
use askama_web::WebTemplate;

use galoy_agents_core::code_assistant::logs::{CodeAssistantRequestRow, DashboardStats};
use galoy_agents_core::code_assistant::LabelOriginCounts;

pub struct AgentView {
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
    pub agents: Vec<AgentView>,
}

#[derive(Template, WebTemplate)]
#[template(path = "agent_created.html")]
pub struct AgentCreatedTemplate {
    pub agent_name: String,
    pub mcp_json: String,
    pub cli_command: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "agent_row.html")]
pub struct AgentRowTemplate {
    pub agent: AgentView,
}

#[derive(Template, WebTemplate)]
#[template(path = "agent_list.html")]
pub struct AgentListTemplate {
    pub agents: Vec<AgentView>,
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

// ── Sandboxes ────────────────────────────────────────────────────────

pub struct SandboxView {
    pub name: String,
    pub sandbox_name: String,
    pub phase: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "sandboxes.html")]
pub struct SandboxesTemplate {
    pub enabled: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "sandbox_list.html")]
pub struct SandboxListTemplate {
    pub sandboxes: Vec<SandboxView>,
}

#[derive(Template, WebTemplate)]
#[template(path = "sandbox_row.html")]
pub struct SandboxRowTemplate {
    pub sb: SandboxView,
}

#[derive(Template, WebTemplate)]
#[template(path = "sandbox_created.html")]
pub struct SandboxCreatedTemplate {
    pub name: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "sandbox_terminal.html")]
pub struct SandboxTerminalTemplate {
    pub name: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "sandbox_agent.html")]
pub struct SandboxAgentTemplate {
    pub name: String,
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
}
