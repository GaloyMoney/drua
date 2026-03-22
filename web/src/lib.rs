pub mod auth;
mod routes;
mod templates;

use axum::Router;

use galoy_agents_domain as domain;

use domain::agent::Agents;
use domain::user::Users;

use auth::config::OAuthClient;

/// Unified application state shared by all routes and middleware.
#[derive(Clone)]
pub struct AppState {
    pub users: Users,
    pub agents: Agents,
    pub oauth_client: OAuthClient,
}

impl AppState {
    pub fn new(users: Users, agents: Agents, oauth_client: OAuthClient) -> Self {
        Self {
            users,
            agents,
            oauth_client,
        }
    }
}

/// Build the web router with page routes and auth routes.
///
/// The returned router is not yet layered with session or auth middleware —
/// the caller (cli) is responsible for adding those layers and calling `.with_state()`.
pub fn router() -> Router<AppState> {
    routes::router().merge(auth::auth_router())
}
