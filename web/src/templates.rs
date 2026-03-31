use askama::Template;
use askama_web::WebTemplate;

use galoy_agents_core::code_assistant::logs::{CodeAssistantRequestRow, DashboardStats};

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

pub struct AuditEntryView {
    pub display_subject: String,
    pub interaction_type: String,
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
