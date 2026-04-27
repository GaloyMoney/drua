//! Webhook ingestion route for workflow triggers.
//!
//! `POST /webhooks/:definition_id` lets external systems (e.g. Honeycomb
//! triggers) kick off a workflow run. The route sits *outside* the normal
//! auth middleware — instead, the request is authenticated against the
//! webhook secret stored on the workflow definition's trigger config.

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
    // 1. Load the workflow definition (bypassing auth — we authenticate
    //    via the trigger secret instead).
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

    // 2. Only webhook-triggered workflows accept POSTs here.
    let (provider, expected_secret) = match &definition.trigger {
        WorkflowTrigger::Webhook { provider, secret } => (provider.clone(), secret.clone()),
        WorkflowTrigger::Manual => {
            return StatusCode::METHOD_NOT_ALLOWED.into_response();
        }
    };

    // 3. Pick the verification header for the provider.
    let header_name = match provider.as_deref() {
        Some("honeycomb") => "x-honeycomb-webhook-token",
        _ => "authorization",
    };

    // 4. Extract & compare the presented secret.
    let presented = match headers.get(header_name).and_then(|v| v.to_str().ok()) {
        Some(value) => match provider.as_deref() {
            // Generic provider: Authorization: Bearer <secret>
            None => value.strip_prefix("Bearer ").unwrap_or(value).to_string(),
            // Provider-specific: header value is the raw secret
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

    // 5. Parse the body as JSON. An empty body is treated as `null`.
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

    // 6. Trigger the run. The executor runs on a background tokio task,
    //    so the webhook returns immediately. We pass the already-loaded
    //    definition to avoid a second `find_by_id` round trip.
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
