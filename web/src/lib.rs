pub mod auth;
mod routes;
mod templates;

use std::sync::Arc;

use axum::Router;

use galoy_agents_domain as domain;

use domain::agent::Agents;
use domain::user::Users;

pub use routes::AppState;

/// Build the web router with page routes.
///
/// The returned router is not yet merged with auth or graphql routes —
/// the caller (cli) is responsible for composing routers.
pub fn router(users: Users, agents: Agents) -> Router {
    let state = Arc::new(AppState { users, agents });
    routes::router(state)
}
