use std::convert::Infallible;

use tower_sessions::{cookie::SameSite, SessionManagerLayer};

use crate::auth::session_store::PgSessionStore;
use crate::AppState;

pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub secure_cookies: bool,
}

/// Build the axum [`Router`] with all web routes, MCP gateway, auth, and
/// session middleware applied.
///
/// The `mcp_service` is the pre-built MCP gateway service (from
/// [`McpGateway::service`]) that will be mounted at `/mcp`.
pub fn build_app<M>(
    config: &ServerConfig,
    pool: &sqlx::PgPool,
    app_state: AppState,
    mcp_service: M,
) -> axum::Router
where
    M: tower_service::Service<axum::extract::Request, Error = Infallible>
        + Clone
        + Send
        + Sync
        + 'static,
    M::Response: axum::response::IntoResponse,
    M::Future: Send + 'static,
{
    let session_store = PgSessionStore::new(pool);
    let session_layer = SessionManagerLayer::new(session_store)
        .with_same_site(SameSite::Lax)
        .with_secure(config.secure_cookies);

    crate::router()
        .nest_service("/mcp", mcp_service)
        .layer(axum::middleware::from_fn(crate::auth::auth_middleware))
        .layer(axum::Extension(app_state.clone()))
        .layer(session_layer)
        .with_state(app_state)
}
