//! Validates Kubernetes projected ServiceAccount tokens via the TokenReview API.
//!
//! Sandbox pods authenticate to the MCP gateway using audience-scoped SA tokens
//! (projected volume, auto-rotated by kubelet). This module validates those tokens
//! and extracts the agent identity from the bound pod name.

use k8s_openapi::api::authentication::v1::{TokenReview, TokenReviewSpec, TokenReviewStatus};
use kube::api::PostParams;
use tracing::instrument;

use super::error::AuthError;

#[derive(Clone)]
pub struct SaTokenValidator {
    kube_client: kube::Client,
    audience: String,
}

impl SaTokenValidator {
    /// Returns `None` outside a K8s cluster (e.g. local dev).
    pub async fn try_from_env(audience: impl Into<String>) -> Option<Self> {
        let client = kube::Client::try_default().await.ok()?;
        Some(Self {
            kube_client: client,
            audience: audience.into(),
        })
    }

    /// Returns the agent UUID parsed from the bound pod name (`agent-{uuid}`).
    #[instrument(name = "web.auth.sa_token.validate", skip_all)]
    pub async fn validate(&self, raw_token: &str) -> Result<String, AuthError> {
        let review = TokenReview {
            spec: TokenReviewSpec {
                token: Some(raw_token.to_string()),
                audiences: Some(vec![self.audience.clone()]),
            },
            ..Default::default()
        };

        let api: kube::Api<TokenReview> = kube::Api::all(self.kube_client.clone());
        let result = api
            .create(&PostParams::default(), &review)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "TokenReview API call failed");
                AuthError::InvalidToken
            })?;

        let status = result.status.ok_or(AuthError::InvalidToken)?;
        if !status.authenticated.unwrap_or(false) {
            return Err(AuthError::InvalidToken);
        }

        let pod_name = extract_pod_name(&status)?;
        let id_str = pod_name
            .strip_prefix("agent-")
            .ok_or(AuthError::InvalidToken)?;
        // Full UUID required to prevent ambiguous prefix lookups.
        id_str
            .parse::<uuid::Uuid>()
            .map_err(|_| AuthError::InvalidToken)?;
        Ok(id_str.to_string())
    }
}

/// Kubelet includes `authentication.kubernetes.io/pod-name` in the bound
/// token's extra fields (nested under `status.user.extra`).
fn extract_pod_name(status: &TokenReviewStatus) -> Result<String, AuthError> {
    let user = status.user.as_ref().ok_or(AuthError::InvalidToken)?;
    let extra = user.extra.as_ref().ok_or(AuthError::InvalidToken)?;
    let pod_names = extra
        .get("authentication.kubernetes.io/pod-name")
        .ok_or(AuthError::InvalidToken)?;
    pod_names.first().cloned().ok_or(AuthError::InvalidToken)
}

/// Quick heuristic: SA tokens are JWTs (three dot-separated base64 segments).
pub(super) fn looks_like_jwt(token: &str) -> bool {
    let parts: Vec<&str> = token.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| !p.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_like_jwt_detects_jwts() {
        assert!(looks_like_jwt(
            "eyJhbGciOiJSUzI1NiJ9.eyJpc3MiOiJrdWJl.c2lnbmF0dXJl"
        ));
        assert!(!looks_like_jwt("abc123plaintoken"));
        assert!(!looks_like_jwt("a.b"));
        assert!(!looks_like_jwt("a..c"));
    }
}
