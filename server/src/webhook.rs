//! Outside the auth middleware — authn via the trigger's stored secret.
//!
//! Two dispatch surfaces:
//! - `POST /webhooks/{definition_id}` — per-definition endpoint (1:1).
//! - `POST /webhooks/providers/{provider}` — fan-out: evaluates every
//!   definition tagged with this provider and spawns runs for those
//!   whose `trigger.condition` passes. Auth via env var
//!   `WEBHOOK_SECRET_{PROVIDER_UPPER}`.
//!
//! Provider `github_app` uses HMAC-SHA256 verification
//! (`X-Hub-Signature-256: sha256=<hex>`) instead of bearer tokens.

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
    Json, Router,
};
use hmac::{Hmac, Mac};
use serde::Serialize;
use sha2::Sha256;
use tracing::instrument;

use drua_core as domain;

use domain::primitives::WorkflowDefinitionId;
use domain::workflow::WorkflowTrigger;

use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/webhooks/{definition_id}", post(handle_webhook))
        .route(
            "/webhooks/providers/{provider}",
            post(handle_provider_webhook),
        )
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
        WorkflowTrigger::Webhook {
            provider, secret, ..
        } => (provider.clone(), secret.clone()),
        WorkflowTrigger::Manual { .. } | WorkflowTrigger::Cron { .. } => {
            return StatusCode::METHOD_NOT_ALLOWED.into_response();
        }
    };

    if !verify_webhook(provider.as_deref(), &expected_secret, &headers, &body) {
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
        Ok(Some(run)) => {
            tracing::info!(run_id = %run.id, "workflow run triggered via webhook");
            (StatusCode::OK, run.id.to_string()).into_response()
        }
        Ok(None) => {
            tracing::info!("webhook accepted; run suppressed by trigger condition");
            (StatusCode::OK, "filtered").into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "webhook: failed to trigger run");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[derive(Serialize)]
struct ProviderFanOutResponse {
    triggered: usize,
    filtered: usize,
    errored: usize,
}

#[instrument(name = "web.webhook.handle_provider", skip_all, fields(%provider))]
pub async fn handle_provider_webhook(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let provider = provider.to_lowercase();
    let env_key = format!(
        "WEBHOOK_SECRET_{}",
        provider.to_uppercase().replace('-', "_")
    );
    let expected_secret = match std::env::var(&env_key) {
        Ok(s) if !s.is_empty() => s,
        _ => {
            tracing::warn!(%provider, env_key, "provider webhook secret not configured");
            return StatusCode::NOT_IMPLEMENTED.into_response();
        }
    };

    if !verify_webhook(Some(&provider), &expected_secret, &headers, &body) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let trigger_context = if body.is_empty() {
        serde_json::Value::Null
    } else {
        match serde_json::from_slice::<serde_json::Value>(&body) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "provider webhook: invalid JSON body");
                return (StatusCode::BAD_REQUEST, "invalid JSON").into_response();
            }
        }
    };

    match state
        .app
        .workflows()
        .trigger_all_for_provider(&provider, trigger_context)
        .await
    {
        Ok(result) => (
            StatusCode::OK,
            Json(ProviderFanOutResponse {
                triggered: result.triggered,
                filtered: result.filtered,
                errored: result.errored,
            }),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "provider webhook: fan-out failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn verify_webhook(
    provider: Option<&str>,
    expected_secret: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> bool {
    match provider {
        Some("github_app") => verify_github_hmac(expected_secret, headers, body),
        other => verify_bearer_or_token(other, expected_secret, headers),
    }
}

/// HMAC-SHA256 verification per GitHub's webhook spec: the
/// `X-Hub-Signature-256` header carries `sha256=<hex>` computed
/// over the raw body with the webhook secret as key.
fn verify_github_hmac(secret: &str, headers: &HeaderMap, body: &[u8]) -> bool {
    let signature = match headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
    {
        Some(s) => s,
        None => {
            tracing::warn!("webhook: missing X-Hub-Signature-256 header");
            return false;
        }
    };

    let hex_sig = match signature.strip_prefix("sha256=") {
        Some(s) => s,
        None => {
            tracing::warn!("webhook: X-Hub-Signature-256 missing sha256= prefix");
            return false;
        }
    };

    let expected_bytes = match hex::decode(hex_sig) {
        Ok(b) => b,
        Err(_) => {
            tracing::warn!("webhook: X-Hub-Signature-256 contains invalid hex");
            return false;
        }
    };

    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(body);

    // `verify_slice` is constant-time.
    mac.verify_slice(&expected_bytes).is_ok()
}

fn verify_bearer_or_token(
    provider: Option<&str>,
    expected_secret: &str,
    headers: &HeaderMap,
) -> bool {
    let header_name = match provider {
        Some("honeycomb") => "x-honeycomb-webhook-token",
        Some("concourse") | None => "authorization",
        _ => "authorization",
    };

    let presented = match headers.get(header_name).and_then(|v| v.to_str().ok()) {
        Some(value) => match provider {
            Some("honeycomb") => value.to_string(),
            _ => value.strip_prefix("Bearer ").unwrap_or(value).to_string(),
        },
        None => {
            tracing::warn!(header_name, "webhook: missing verification header");
            return false;
        }
    };

    if presented != expected_secret {
        tracing::warn!("webhook: secret mismatch");
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_default_provider() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer whsec_abc".parse().unwrap());
        assert!(verify_webhook(None, "whsec_abc", &headers, b""));
    }

    #[test]
    fn bearer_concourse_provider() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer whsec_abc".parse().unwrap());
        assert!(verify_webhook(
            Some("concourse"),
            "whsec_abc",
            &headers,
            b""
        ));
    }

    #[test]
    fn honeycomb_custom_header() {
        let mut headers = HeaderMap::new();
        headers.insert("x-honeycomb-webhook-token", "whsec_abc".parse().unwrap());
        assert!(verify_webhook(
            Some("honeycomb"),
            "whsec_abc",
            &headers,
            b""
        ));
    }

    #[test]
    fn bearer_mismatch_rejects() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer wrong".parse().unwrap());
        assert!(!verify_webhook(None, "whsec_abc", &headers, b""));
    }

    #[test]
    fn missing_header_rejects() {
        let headers = HeaderMap::new();
        assert!(!verify_webhook(None, "whsec_abc", &headers, b""));
    }

    #[test]
    fn unknown_provider_falls_back_to_authorization() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "whsec_abc".parse().unwrap());
        assert!(verify_webhook(Some("unknown"), "whsec_abc", &headers, b""));
    }

    #[test]
    fn unknown_provider_strips_bearer() {
        assert!(verify_bearer_or_token(Some("github"), "my-secret", &{
            let mut h = HeaderMap::new();
            h.insert("authorization", "Bearer my-secret".parse().unwrap());
            h
        }));
    }

    #[test]
    fn github_app_hmac_valid_signature() {
        let secret = "whsec_test_secret";
        let body = b"{\"action\":\"opened\"}";

        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let signature = hex::encode(mac.finalize().into_bytes());

        let mut headers = HeaderMap::new();
        headers.insert(
            "x-hub-signature-256",
            format!("sha256={signature}").parse().unwrap(),
        );
        assert!(verify_webhook(Some("github_app"), secret, &headers, body));
    }

    #[test]
    fn github_app_hmac_wrong_signature_rejects() {
        let mut headers = HeaderMap::new();
        headers.insert("x-hub-signature-256", "sha256=deadbeef".parse().unwrap());
        assert!(!verify_webhook(
            Some("github_app"),
            "whsec_abc",
            &headers,
            b"{}"
        ));
    }

    #[test]
    fn github_app_hmac_missing_header_rejects() {
        let headers = HeaderMap::new();
        assert!(!verify_webhook(
            Some("github_app"),
            "whsec_abc",
            &headers,
            b"{}"
        ));
    }

    #[test]
    fn github_app_hmac_missing_prefix_rejects() {
        let mut headers = HeaderMap::new();
        headers.insert("x-hub-signature-256", "deadbeef".parse().unwrap());
        assert!(!verify_webhook(
            Some("github_app"),
            "whsec_abc",
            &headers,
            b"{}"
        ));
    }

    #[test]
    fn github_app_hmac_invalid_hex_rejects() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-hub-signature-256",
            "sha256=not_valid_hex!".parse().unwrap(),
        );
        assert!(!verify_webhook(
            Some("github_app"),
            "whsec_abc",
            &headers,
            b"{}"
        ));
    }

    #[test]
    fn bearer_without_prefix_still_matches() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "whsec_abc".parse().unwrap());
        assert!(verify_webhook(None, "whsec_abc", &headers, b""));
    }
}
