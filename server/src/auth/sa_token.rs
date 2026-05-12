//! Validates Kubernetes projected ServiceAccount tokens via the TokenReview API.
//!
//! The git-proxy doesn't gate on per-pod identity — a valid
//! audience-scoped JWT from any sandbox pod is enough to use the proxy
//! (the global allow-list constrains which repos / refs are reachable).
//! `validate` therefore returns nothing: it just confirms the JWT
//! validates for the configured audience, no pod-name parsing.

use k8s_openapi::api::authentication::v1::{TokenReview, TokenReviewSpec};
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

    /// Validates the SA token against the K8s TokenReview API for the
    /// configured audience. Returns `Ok(())` on success; the bound
    /// subject's identity isn't propagated — the global git-proxy
    /// allow-list is the policy gate.
    #[instrument(name = "web.auth.sa_token.validate", skip_all)]
    pub async fn validate(&self, raw_token: &str) -> Result<(), AuthError> {
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
        Ok(())
    }
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
