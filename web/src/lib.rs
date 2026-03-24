pub mod auth;
mod routes;
pub mod server;
mod templates;

use axum::Router;

use galoy_agents_domain as domain;

use domain::App;

use auth::config::OAuthClient;

/// Unified application state shared by all routes and middleware.
#[derive(Clone)]
pub struct AppState {
    pub app: App,
    pub oauth_client: OAuthClient,
    pub mcp_endpoint: String,
}

impl AppState {
    pub fn new(app: App, oauth_client: OAuthClient, mcp_endpoint: String) -> Self {
        Self {
            app,
            oauth_client,
            mcp_endpoint,
        }
    }
}

/// Build the web router with page routes and auth routes.
pub fn router() -> Router<AppState> {
    routes::router().merge(auth::auth_router())
}
