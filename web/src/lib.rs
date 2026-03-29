pub mod auth;
mod routes;
pub mod server;
mod templates;

use std::sync::Arc;

use axum::Router;

use galoy_agents_core as domain;

use domain::App;

use auth::config::OAuthClient;
use code_assistant_server::CodeAssistantEndpoints;
use sandbox_client::SandboxClient;

/// Unified application state shared by all routes and middleware.
#[derive(Clone)]
pub struct AppState {
    pub app: App,
    pub oauth_client: OAuthClient,
    pub mcp_endpoint: String,
    pub github_allowed_teams: Vec<String>,
    pub code_assistant: Option<CodeAssistantEndpoints>,
    pub sandbox: Option<Arc<SandboxClient>>,
}

impl AppState {
    pub fn new(
        app: App,
        oauth_client: OAuthClient,
        mcp_endpoint: String,
        github_allowed_teams: Vec<String>,
        code_assistant: Option<CodeAssistantEndpoints>,
        sandbox: Option<SandboxClient>,
    ) -> Self {
        Self {
            app,
            oauth_client,
            mcp_endpoint,
            github_allowed_teams,
            code_assistant,
            sandbox: sandbox.map(Arc::new),
        }
    }
}

/// Build the web router with page routes, auth routes, and API routes.
pub fn router() -> Router<AppState> {
    routes::router()
        .merge(auth::auth_router())
        .merge(routes::api_router())
}
