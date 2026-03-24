use tower_sessions::{cookie::SameSite, SessionManagerLayer};
use tracing::instrument;

use crate::auth::session_store::PgSessionStore;
use crate::AppState;

pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub secure_cookies: bool,
}

#[instrument(name = "web.server.run", skip_all, fields(addr))]
pub async fn run(
    config: ServerConfig,
    pool: &sqlx::PgPool,
    app_state: AppState,
) -> anyhow::Result<()> {
    let session_store = PgSessionStore::new(pool);
    let session_layer = SessionManagerLayer::new(session_store)
        .with_same_site(SameSite::Lax)
        .with_secure(config.secure_cookies);

    let app = crate::router()
        .layer(axum::middleware::from_fn(crate::auth::auth_middleware))
        .layer(axum::Extension(app_state.clone()))
        .layer(session_layer)
        .with_state(app_state);

    let addr: std::net::SocketAddr = format!("{}:{}", config.host, config.port)
        .parse()
        .expect("invalid bind address");
    tracing::info!("Starting server on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
