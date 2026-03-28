pub mod auth;
mod routes;
pub mod server;
pub mod task_runner;
mod templates;

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use tokio::sync::RwLock;

use galoy_agents_core as domain;

use domain::primitives::TaskId;
use domain::App;

use auth::config::OAuthClient;
use sandbox_client::SandboxClient;
use style_agent_server::StyleAgentEndpoints;

/// In-memory storage for per-task GitHub tokens (not persisted).
pub type TaskGithubTokens = Arc<RwLock<HashMap<TaskId, String>>>;

/// Unified application state shared by all routes and middleware.
#[derive(Clone)]
pub struct AppState {
    pub app: App,
    pub oauth_client: OAuthClient,
    pub mcp_endpoint: String,
    pub github_allowed_teams: Vec<String>,
    pub style_agent: Option<StyleAgentEndpoints>,
    pub sandbox: Option<Arc<SandboxClient>>,
    pub task_github_tokens: TaskGithubTokens,
}

impl AppState {
    pub fn new(
        app: App,
        oauth_client: OAuthClient,
        mcp_endpoint: String,
        github_allowed_teams: Vec<String>,
        style_agent: Option<StyleAgentEndpoints>,
        sandbox: Option<SandboxClient>,
    ) -> Self {
        Self {
            app,
            oauth_client,
            mcp_endpoint,
            github_allowed_teams,
            style_agent,
            sandbox: sandbox.map(Arc::new),
            task_github_tokens: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Store a GitHub token for a task (called at task creation time).
    pub fn set_task_github_token(&self, task_id: TaskId, token: String) {
        let tokens = self.task_github_tokens.clone();
        tokio::spawn(async move {
            tokens.write().await.insert(task_id, token);
        });
    }

    /// Retrieve and remove a GitHub token for a task (called by runner).
    pub async fn take_task_github_token(&self, task_id: TaskId) -> Option<String> {
        self.task_github_tokens.write().await.remove(&task_id)
    }
}

/// Build the web router with page routes, auth routes, and API routes.
pub fn router() -> Router<AppState> {
    routes::router()
        .merge(auth::auth_router())
        .merge(routes::api_router())
}
