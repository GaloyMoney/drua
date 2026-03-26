pub mod auth;
mod routes;
pub mod server;
mod templates;

use axum::Router;

use galoy_agents_domain as domain;

use domain::App;

use auth::config::OAuthClient;
use style_agent_server::StyleAgentEndpoints;

/// Unified application state shared by all routes and middleware.
#[derive(Clone)]
pub struct AppState {
    pub app: App,
    pub oauth_client: OAuthClient,
    pub mcp_endpoint: String,
    pub github_allowed_teams: Vec<String>,
    pub style_agent: Option<StyleAgentEndpoints>,
}

impl AppState {
    pub fn new(
        app: App,
        oauth_client: OAuthClient,
        mcp_endpoint: String,
        github_allowed_teams: Vec<String>,
        style_agent: Option<StyleAgentEndpoints>,
    ) -> Self {
        Self {
            app,
            oauth_client,
            mcp_endpoint,
            github_allowed_teams,
            style_agent,
        }
    }
}

/// Build the web router with page routes and auth routes.
pub fn router() -> Router<AppState> {
    routes::router().merge(auth::auth_router())
}
