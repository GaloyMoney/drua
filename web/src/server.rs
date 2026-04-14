use std::convert::Infallible;

use axum::{extract::Request, middleware::Next, response::Response};
use opentelemetry::global;
use opentelemetry_http::HeaderExtractor;
use tower_sessions::{cookie::SameSite, SessionManagerLayer};
use tracing::instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use galoy_agents_core::audit::primitives::{InteractionOutcome, InteractionType};
use galoy_agents_core::audit::Audit;
use galoy_agents_core::auth::AuthSubject;

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

/// Post-response middleware that records an audit entry for every mutating
/// HTTP request (POST / PUT / DELETE). Read-only requests are skipped.
///
/// Seeds an [`EventContext`] and uses the type-safe [`Audit::record_*`]
/// helpers to accumulate audit fields. The context is propagated via
/// [`WithEventContext`] so downstream handlers and services can enrich it.
/// After the handler completes the collected context is persisted
/// fire-and-forget.
#[instrument(name = "web.audit.middleware", skip_all)]
async fn audit_middleware(request: Request, next: Next) -> Response {
    use axum::http::Method;
    use es_entity::context::{EventContext, WithEventContext};

    let method = request.method().clone();
    if method == Method::GET || method == Method::HEAD || method == Method::OPTIONS {
        return next.run(request).await;
    }

    let auth = request.extensions().get::<AuthSubject>().cloned();
    let app_state = request.extensions().get::<AppState>().cloned();
    let path = request.uri().path().to_string();

    // Obtain an empty seed — the `!Send` EventContext must not live across
    // an `.await`, so we scope it in a block.
    let seed_data = {
        let ctx = EventContext::current();
        ctx.data()
    };

    async {
        Audit::record_interaction_type(InteractionType::ApiCall);
        Audit::record_action(format!("{} {}", method, path));
        Audit::record_metadata(serde_json::json!({ "method": method.as_str(), "path": path }));
        if let Some(ref auth) = auth {
            Audit::record_subject(auth);
        }

        let start = std::time::Instant::now();
        let response = next.run(request).await;
        Audit::record_duration(start);

        let status = response.status();
        if status.is_success() || status.is_redirection() {
            // Use _if_unset so inner handlers (e.g. MCP tool errors
            // returned as HTTP 200) are not overwritten.
            Audit::record_outcome_if_unset(InteractionOutcome::Success);
        } else if status == axum::http::StatusCode::UNAUTHORIZED
            || status == axum::http::StatusCode::FORBIDDEN
        {
            Audit::record_error("unauthorized");
        } else {
            Audit::record_error(status.to_string());
        }

        if let Some(state) = app_state {
            state.app.audit().record_from_context();
        }

        response
    }
    .with_event_context(seed_data)
    .await
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
        .layer(axum::middleware::from_fn(audit_middleware))
        .layer(axum::middleware::from_fn(trace_context_middleware))
        .layer(axum::middleware::from_fn(crate::auth::auth_middleware))
        .layer(axum::Extension(app_state.clone()))
        .layer(session_layer)
        .with_state(app_state)
}
