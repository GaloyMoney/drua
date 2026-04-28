//! Outside the auth middleware — authn via the trigger's stored secret.

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
    Router,
};
use tracing::instrument;

use drua_core as domain;

use domain::primitives::WorkflowDefinitionId;
use domain::workflow::WorkflowTrigger;

use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/webhooks/{definition_id}", post(handle_webhook))
}

#[instrument(name = "web.webhook.handle", skip_all, fields(%definition_id))]
pub async fn handle_webhook(
    State(state): State<AppState>,
    Path(definition_id): Path<WorkflowDefinitionId>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let definition = match state
        .app
        .workflows()
        .find_by_id_unchecked(definition_id)
        .await
    {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %e, "webhook: definition not found");
            return StatusCode::NOT_FOUND.into_response();
        }
    };

    let (provider, expected_secret) = match &definition.trigger {
        WorkflowTrigger::Webhook { provider, secret } => (provider.clone(), secret.clone()),
        WorkflowTrigger::Manual => {
            return StatusCode::METHOD_NOT_ALLOWED.into_response();
        }
    };

    let header_name = match provider.as_deref() {
        Some("honeycomb") => "x-honeycomb-webhook-token",
        _ => "authorization",
    };

    let presented = match headers.get(header_name).and_then(|v| v.to_str().ok()) {
        Some(value) => match provider.as_deref() {
            // Generic provider strips the `Bearer ` prefix; named
            // providers carry the raw secret.
            None => value.strip_prefix("Bearer ").unwrap_or(value).to_string(),
            _ => value.to_string(),
        },
        None => {
            tracing::warn!(header_name, "webhook: missing verification header");
            return StatusCode::UNAUTHORIZED.into_response();
        }
    };

    if presented != expected_secret {
        tracing::warn!("webhook: secret mismatch");
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let trigger_context = if body.is_empty() {
        serde_json::Value::Null
    } else {
        match serde_json::from_slice::<serde_json::Value>(&body) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "webhook: invalid JSON body");
                return (StatusCode::BAD_REQUEST, "invalid JSON").into_response();
            }
        }
    };

    match state
        .app
        .workflows()
        .trigger_run_for_definition(definition, trigger_context)
        .await
    {
        Ok(run) => {
            tracing::info!(run_id = %run.id, "workflow run triggered via webhook");
            (StatusCode::OK, run.id.to_string()).into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "webhook: failed to trigger run");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
