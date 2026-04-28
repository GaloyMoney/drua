use std::convert::Infallible;

use axum::{extract::Request, middleware::Next, response::Response};
use opentelemetry::global;
use opentelemetry_http::HeaderExtractor;
use tower_sessions::{cookie::SameSite, SessionManagerLayer};
use tracing::instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use drua_core::audit::primitives::{InteractionOutcome, InteractionType};
use drua_core::audit::Audit;
use drua_core::auth::AuthSubject;
use drua_core::primitives::WorkspaceId;

use crate::AppState;

/// Connects ingress → server spans by extracting W3C traceparent.
async fn trace_context_middleware(request: Request, next: Next) -> Response {
    let parent_cx = global::get_text_map_propagator(|propagator| {
        propagator.extract(&HeaderExtractor(request.headers()))
    });
    let _ = tracing::Span::current().set_parent(parent_cx);
    next.run(request).await
}

/// Records an audit entry for every mutating HTTP request. Seeds an
/// [`EventContext`] propagated via [`WithEventContext`] so downstream
/// handlers can enrich it; persists fire-and-forget on response.
#[instrument(name = "web.audit.middleware", skip_all)]
async fn audit_middleware(request: Request, next: Next) -> Response {
    use axum::http::Method;
    use es_entity::context::{EventContext, WithEventContext};

    let method = request.method().clone();
    let path = request.uri().path().to_string();
    if method == Method::GET || method == Method::HEAD || method == Method::OPTIONS {
        return next.run(request).await;
    }
    // MCP gateway spawns its own tokio tasks so the EventContext does
    // not propagate. The gateway handles audit internally.
    // Use exact match — `/mcp-creds/…` routes must NOT be skipped.
    if path == "/mcp" || path.starts_with("/mcp/") {
        return next.run(request).await;
    }

    let auth = request.extensions().get::<AuthSubject>().cloned();
    let app_state = request.extensions().get::<AppState>().cloned();

    // The `!Send` EventContext must not live across an `.await`.
    let seed_data = {
        let ctx = EventContext::current();
        ctx.data()
    };

    async {
        Audit::record_interaction_type(InteractionType::ApiCall);
        Audit::record_entrypoint(format!("api: {} {}", method, path));
        Audit::record_metadata(serde_json::json!({ "method": method.as_str(), "path": path }));
        if let Some(ref auth) = auth {
            Audit::record_subject(auth);
        }
        if let Some(ws_id) = extract_workspace_id_from_path(&path) {
            Audit::record_workspace_id(ws_id);
        }

        let start = std::time::Instant::now();
        let response = next.run(request).await;
        Audit::record_duration(start);

        // Don't overwrite a more specific outcome already recorded by a handler.
        let status = response.status();
        let fallback = if status.is_success() || status.is_redirection() {
            InteractionOutcome::Success
        } else if status == axum::http::StatusCode::UNAUTHORIZED
            || status == axum::http::StatusCode::FORBIDDEN
        {
            InteractionOutcome::Error {
                message: "unauthorized".to_string(),
            }
        } else {
            InteractionOutcome::Error {
                message: status.to_string(),
            }
        };
        Audit::record_outcome_if_unset(fallback);

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

/// `mcp_service` is the pre-built MCP gateway service (from
/// [`McpGateway::service`]); it is mounted at `/mcp`.
pub fn build_app<M>(config: &ServerConfig, app_state: AppState, mcp_service: M) -> axum::Router
where
    M: tower_service::Service<axum::extract::Request, Error = Infallible>
        + Clone
        + Send
        + Sync
        + 'static,
    M::Response: axum::response::IntoResponse,
    M::Future: Send + 'static,
{
    let session_layer = SessionManagerLayer::new(app_state.session_store.clone())
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

/// Matches `/workspaces/{uuid}/…` or `/api/v1/workspaces/{uuid}/…`.
fn extract_workspace_id_from_path(path: &str) -> Option<WorkspaceId> {
    let segments: Vec<&str> = path.split('/').collect();
    segments
        .iter()
        .position(|s| *s == "workspaces")
        .and_then(|i| segments.get(i + 1))
        .and_then(|s| s.parse::<uuid::Uuid>().ok())
        .map(WorkspaceId::from)
}
