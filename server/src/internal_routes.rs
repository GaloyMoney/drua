use axum::{
    extract::{Path, Query, Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use drua_core::tunnel::{wire::CallToolResult, InternalCallReq};
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
    let auth_header = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    match internal_auth_status(auth_header, configured_secret.as_deref()) {
        Ok(()) => next.run(req).await,
        Err(StatusCode::SERVICE_UNAVAILABLE) => {
            tracing::warn!("/internal/tunnel called but no internal_secret configured");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
        Err(status) => status.into_response(),
    }
}

fn internal_auth_status(
    auth_header: Option<&str>,
    configured_secret: Option<&str>,
) -> Result<(), StatusCode> {
    let configured_secret = match configured_secret {
        Some(s) if !s.is_empty() => s,
        _ => return Err(StatusCode::SERVICE_UNAVAILABLE),
    };

    let presented = auth_header.and_then(|v| v.strip_prefix("Bearer "));
    match presented {
        Some(s) if constant_time_eq(s.as_bytes(), configured_secret.as_bytes()) => Ok(()),
        _ => Err(StatusCode::UNAUTHORIZED),
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
struct InternalCallParams {
    session_id: uuid::Uuid,
}

#[instrument(name = "web.internal.tunnel_call", skip_all, fields(deployment_id = %deployment_id))]
async fn internal_tunnel_call(
    State(state): State<AppState>,
    Path(deployment_id): Path<String>,
    Query(params): Query<InternalCallParams>,
    Json(req): Json<InternalCallReq>,
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

    if params.session_id != local_session {
        tracing::info!(
            deployment_id = %deployment_id,
            expected_session = %params.session_id,
            local_session = %local_session,
            "rejecting stale proxy call (session_id mismatch)"
        );
        return (
            StatusCode::GONE,
            format!(
                "tunnel '{deployment_id}' session_id mismatch: expected {}, have {local_session}",
                params.session_id
            ),
        )
            .into_response();
    }

    match handle
        .call_tool(&req.upstream, &req.tool_name, req.arguments)
        .await
    {
        Ok(result) => Json::<CallToolResult>(result).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_auth_rejects_missing_secret() {
        assert_eq!(
            internal_auth_status(Some("Bearer secret"), None),
            Err(StatusCode::SERVICE_UNAVAILABLE)
        );
        assert_eq!(
            internal_auth_status(Some("Bearer secret"), Some("")),
            Err(StatusCode::SERVICE_UNAVAILABLE)
        );
    }

    #[test]
    fn internal_auth_rejects_missing_or_wrong_header() {
        assert_eq!(
            internal_auth_status(None, Some("secret")),
            Err(StatusCode::UNAUTHORIZED)
        );
        assert_eq!(
            internal_auth_status(Some("Bearer wrong"), Some("secret")),
            Err(StatusCode::UNAUTHORIZED)
        );
        assert_eq!(
            internal_auth_status(Some("Basic secret"), Some("secret")),
            Err(StatusCode::UNAUTHORIZED)
        );
    }

    #[test]
    fn internal_auth_accepts_matching_bearer() {
        assert_eq!(
            internal_auth_status(Some("Bearer secret"), Some("secret")),
            Ok(())
        );
    }

    #[test]
    fn internal_call_params_require_session_id() {
        assert!(serde_urlencoded::from_str::<InternalCallParams>("").is_err());
        let session_id = uuid::Uuid::new_v4();
        let parsed =
            serde_urlencoded::from_str::<InternalCallParams>(&format!("session_id={session_id}"))
                .expect("session_id parses");
        assert_eq!(parsed.session_id, session_id);
    }
}
