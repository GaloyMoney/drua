use askama::Template;
use askama_web::WebTemplate;

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
