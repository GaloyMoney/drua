use axum::{
    extract::{Path, Query, Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use drua_core::tunnel::wire::{CallToolResult, JsonObject};
use serde::Deserialize;
use tracing::instrument;

use crate::AppState;

pub fn internal_router() -> Router<AppState> {
    Router::new()
        .route(
            "/internal/tunnel/{deployment_id}/call",
            post(internal_tunnel_call),
        )
        .layer(axum::middleware::from_fn(internal_auth_middleware))
}

#[instrument(name = "web.internal.auth", skip_all)]
async fn internal_auth_middleware(req: Request, next: Next) -> Response {
    let state = match req.extensions().get::<AppState>().cloned() {
        Some(s) => s,
        None => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let configured_secret = state.app.tunnel_runtime().internal_secret.clone();
    let configured_secret = match configured_secret.as_deref() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            tracing::warn!("/internal/tunnel called but no internal_secret configured");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };

    let presented = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match presented {
        Some(s) if constant_time_eq(s.as_bytes(), configured_secret.as_bytes()) => {
            next.run(req).await
        }
        _ => StatusCode::UNAUTHORIZED.into_response(),
    }
}

/// Constant-time equality for internal bearer auth.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[derive(Deserialize)]
struct InternalCallReqOwned {
    upstream: String,
    tool_name: String,
    #[serde(default)]
    arguments: Option<JsonObject>,
}

#[derive(Deserialize)]
struct InternalCallParams {
    session_id: Option<uuid::Uuid>,
}

#[instrument(name = "web.internal.tunnel_call", skip_all, fields(deployment_id = %deployment_id))]
async fn internal_tunnel_call(
    State(state): State<AppState>,
    Path(deployment_id): Path<String>,
    Query(params): Query<InternalCallParams>,
    Json(req): Json<InternalCallReqOwned>,
) -> Response {
    let (local_session, handle) = match state.app.tunnels().local_session(&deployment_id) {
        Some(t) => t,
        None => {
            return (
                StatusCode::NOT_FOUND,
                format!("no live tunnel for deployment '{deployment_id}'"),
            )
                .into_response();
        }
    };

    if let Some(expected) = params.session_id {
        if expected != local_session {
            tracing::info!(
                deployment_id = %deployment_id,
                expected_session = %expected,
                local_session = %local_session,
                "rejecting stale proxy call (session_id mismatch)"
            );
            return (
                StatusCode::GONE,
                format!(
                    "tunnel '{deployment_id}' session_id mismatch: expected {expected}, have {local_session}"
                ),
            )
                .into_response();
        }
    }

    match handle
        .call_tool(&req.upstream, &req.tool_name, req.arguments)
        .await
    {
        Ok(result) => Json::<CallToolResult>(result).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    }
}
