//! Outside the auth middleware — authn via the trigger's stored secret.
//!
//! Recognised `provider` values on `WorkflowTrigger::Webhook`:
//! - `None` / `Some("concourse")` — `Authorization: Bearer <secret>`.
//!   The `concourse` arm is documentation-only; it tags workflows
//!   meant to be fired by the `concourse-drua-resource` so the
//!   dashboard and library YAML carry intent.
//! - `Some("honeycomb")` — `X-Honeycomb-Webhook-Token: <secret>`
//!   (Honeycomb's non-standard header, no `Bearer` prefix).

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

    let header_name = header_name_for_provider(provider.as_deref());

    let presented = match headers.get(header_name).and_then(|v| v.to_str().ok()) {
        Some(value) => extract_secret(value, provider.as_deref()),
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

fn header_name_for_provider(provider: Option<&str>) -> &'static str {
    match provider {
        Some("honeycomb") => "x-honeycomb-webhook-token",
        // `concourse` is documentation-only — same wire shape as `None`.
        Some("concourse") | None => "authorization",
        _ => "authorization",
    }
}

fn extract_secret(header_value: &str, provider: Option<&str>) -> String {
    match provider {
        Some("concourse") | None => header_value
            .strip_prefix("Bearer ")
            .unwrap_or(header_value)
            .to_string(),
        _ => header_value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_name_default_is_authorization() {
        assert_eq!(header_name_for_provider(None), "authorization");
    }

    #[test]
    fn header_name_honeycomb_uses_custom_header() {
        assert_eq!(
            header_name_for_provider(Some("honeycomb")),
            "x-honeycomb-webhook-token"
        );
    }

    #[test]
    fn header_name_concourse_uses_authorization() {
        assert_eq!(header_name_for_provider(Some("concourse")), "authorization");
    }

    #[test]
    fn header_name_unknown_provider_falls_back_to_authorization() {
        assert_eq!(
            header_name_for_provider(Some("not-a-known-provider")),
            "authorization"
        );
    }

    #[test]
    fn extract_secret_strips_bearer_for_default_provider() {
        assert_eq!(extract_secret("Bearer whsec_abc", None), "whsec_abc");
    }

    #[test]
    fn extract_secret_strips_bearer_for_concourse() {
        assert_eq!(
            extract_secret("Bearer whsec_abc", Some("concourse")),
            "whsec_abc"
        );
    }

    #[test]
    fn extract_secret_passes_raw_for_honeycomb() {
        assert_eq!(
            extract_secret("whsec_abc", Some("honeycomb")),
            "whsec_abc"
        );
    }

    #[test]
    fn extract_secret_returns_value_unchanged_when_no_bearer_prefix() {
        assert_eq!(extract_secret("whsec_abc", None), "whsec_abc");
        assert_eq!(extract_secret("whsec_abc", Some("concourse")), "whsec_abc");
    }
}
