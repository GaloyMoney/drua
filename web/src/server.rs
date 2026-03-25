use tower_sessions::{cookie::SameSite, SessionManagerLayer};
use tracing::instrument;

use galoy_agents_mcp_gateway::McpGateway;

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
/// Returns a `Router<()>` that can be extended (e.g. with
/// [`axum::Router::nest`]) before binding to a listener.
pub fn build_app(config: &ServerConfig, pool: &sqlx::PgPool, app_state: AppState) -> axum::Router {
    let session_store = PgSessionStore::new(pool);
    let session_layer = SessionManagerLayer::new(session_store)
        .with_same_site(SameSite::Lax)
        .with_secure(config.secure_cookies);

    let mcp_service = McpGateway::service(app_state.app.clone());

    crate::router()
        .nest_service("/mcp", mcp_service)
        .layer(axum::middleware::from_fn(crate::auth::auth_middleware))
        .layer(axum::Extension(app_state.clone()))
        .layer(session_layer)
        .with_state(app_state)
}

#[instrument(name = "web.server.run", skip_all, fields(addr))]
pub async fn run(
    config: ServerConfig,
    pool: &sqlx::PgPool,
    app_state: AppState,
) -> anyhow::Result<()> {
    let app = build_app(&config, pool, app_state);

    let addr: std::net::SocketAddr = format!("{}:{}", config.host, config.port)
        .parse()
        .expect("invalid bind address");
    tracing::info!("Starting server on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
