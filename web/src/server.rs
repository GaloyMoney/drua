use std::convert::Infallible;

use axum::{extract::Request, middleware::Next, response::Response};
use opentelemetry::global;
use opentelemetry_http::HeaderExtractor;
use tower_sessions::{cookie::SameSite, SessionManagerLayer};
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::auth::session_store::PgSessionStore;
use crate::AppState;

/// Extract W3C traceparent from incoming HTTP headers and attach to
/// the current tracing span. This connects ingress → server spans
/// in the distributed trace.
async fn trace_context_middleware(request: Request, next: Next) -> Response {
    let parent_cx = global::get_text_map_propagator(|propagator| {
        propagator.extract(&HeaderExtractor(request.headers()))
    });
    let _ = tracing::Span::current().set_parent(parent_cx);
    next.run(request).await
}

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
        .layer(axum::middleware::from_fn(trace_context_middleware))
        .layer(axum::middleware::from_fn(crate::auth::auth_middleware))
        .layer(axum::Extension(app_state.clone()))
        .layer(session_layer)
        .with_state(app_state)
}
