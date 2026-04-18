pub mod auth;
pub mod graphql;
mod routes;
pub mod server;
mod templates;

use axum::Router;

use galoy_agents_core as domain;

use domain::App;

use auth::config::{LoginMethod, OAuthClient};
use auth::sa_token::SaTokenValidator;
use domain::code_assistant::CodeAssistant;

/// Unified application state shared by all routes and middleware.
#[derive(Clone)]
pub struct AppState {
    pub app: App,
    pub oauth_client: OAuthClient,
    pub login: LoginMethod,
    pub mcp_endpoint: String,
    pub github_allowed_teams: Vec<String>,
    pub code_assistant: Option<CodeAssistant>,
    /// Optional SA token validator — present when running in-cluster.
    pub sa_token_validator: Option<SaTokenValidator>,
}

impl AppState {
    pub fn new(
        app: App,
        oauth_client: OAuthClient,
        login: LoginMethod,
        mcp_endpoint: String,
        github_allowed_teams: Vec<String>,
    ) -> Self {
        let code_assistant = app.code_assistant().cloned();
        Self {
            app,
            oauth_client,
            login,
            mcp_endpoint,
            github_allowed_teams,
            code_assistant,
            sa_token_validator: None,
        }
    }

    /// Set the SA token validator (typically initialized from in-cluster config).
    pub fn with_sa_token_validator(mut self, validator: SaTokenValidator) -> Self {
        self.sa_token_validator = Some(validator);
        self
    }
}

/// Build the web router with page routes, auth routes, API routes, and GraphQL.
pub fn router() -> Router<AppState> {
    routes::router()
        .merge(auth::auth_router())
        .merge(routes::api_router())
        .merge(graphql::router())
}
