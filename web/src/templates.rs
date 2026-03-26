use askama::Template;
use askama_web::WebTemplate;

use galoy_agents_domain::style_agent_logs::{DashboardStats, StyleAgentRequestRow};

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
#[template(path = "style_agent.html")]
pub struct StyleAgentTemplate {
    pub stats: DashboardStats,
}

#[derive(Template, WebTemplate)]
#[template(path = "style_agent_recent.html")]
pub struct StyleAgentRecentTemplate {
    pub rows: Vec<StyleAgentRequestRow>,
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
#[template(path = "style_agent_search_results.html")]
pub struct StyleAgentSearchResultsTemplate {
    pub query: String,
    pub results: Vec<SearchResultView>,
    pub error: Option<String>,
}
