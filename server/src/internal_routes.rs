//! `/internal/tunnel/:deployment_id/call` — peer-pod proxy receiver.
//!
//! When a `ProxyTunnelToolSet` on a peer pod dispatches a tool call,
//! it lands here. We look up the local in-process `TunnelHandle` (this
//! pod owns the WS) and forward through it. Auth is shared-secret-only
//! in v1 — see `core/src/lib.rs::internal_auth_from_runtime` for the
//! decision.

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

use crate::AppState;

pub fn internal_router() -> Router<AppState> {
    Router::new()
        .route(
            "/internal/tunnel/{deployment_id}/call",
            post(internal_tunnel_call),
        )
        .layer(axum::middleware::from_fn(internal_auth_middleware))
}

/// Bearer-token auth against `tunnel.internal_secret`. The shared
/// secret is loaded from Helm `secrets.tunnelInternalSecret` /
/// `DRUA_TUNNEL_INTERNAL_SECRET`. Endpoints under this middleware run
/// *outside* the user-facing `auth_middleware` chain so a session
/// cookie isn't required.
async fn internal_auth_middleware(req: Request, next: Next) -> Response {
    let state = match req.extensions().get::<AppState>().cloned() {
        Some(s) => s,
        None => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let configured_secret = state.app.tunnel_runtime().internal_secret.clone();
    let configured_secret = match configured_secret.as_deref() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            // Internal route is only meaningful when a secret is
            // configured. Refuse with 503 so a misconfigured deploy
            // surfaces immediately rather than silently 401-ing every
            // proxy hop.
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

/// Constant-time equality so timing attacks can't iteratively recover
/// the shared secret. `subtle` would be the typed dependency, but the
/// secret comparison is small and one-shot — open-coded keeps the
/// dep graph clean.
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

/// Mirrors `drua_core::tunnel::InternalCallReq` over the wire. We
/// re-declare here so the route handler's contract is co-located with
/// its router; the proxy side serializes from the core type.
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

async fn internal_tunnel_call(
    State(state): State<AppState>,
    Path(deployment_id): Path<String>,
    Query(params): Query<InternalCallParams>,
    Json(req): Json<InternalCallReqOwned>,
) -> Response {
    let (local_session, handle) = match state.app.tunnels().local_session(&deployment_id) {
        Some(t) => t,
        None => {
            // Either the connector disconnected on this pod between
            // the peer's catalog read and the POST, or the peer is
            // routing to us based on a stale row. Return 404 so the
            // proxy bubbles up `ToolSetsError::Tunnel(...)`.
            return (
                StatusCode::NOT_FOUND,
                format!("no live tunnel for deployment '{deployment_id}'"),
            )
                .into_response();
        }
    };

    // Session-id fencing: if the proxy was built against a session
    // that has since been displaced on this pod, refuse to dispatch.
    // 410 Gone signals the proxy that its row is stale; the listener
    // will refresh shortly via `pg_notify` or the next reconcile.
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
