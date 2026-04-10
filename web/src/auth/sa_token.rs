//! Validates Kubernetes projected ServiceAccount tokens via the TokenReview API.
//!
//! Sandbox pods authenticate to the MCP gateway using audience-scoped SA tokens
//! (projected volume, auto-rotated by kubelet). This module validates those tokens
//! and extracts the agent identity from the bound pod name.

use k8s_openapi::api::authentication::v1::{TokenReview, TokenReviewSpec, TokenReviewStatus};
use kube::api::PostParams;
use tracing::instrument;

use galoy_agents_core::primitives::AgentId;

use super::error::AuthError;

/// Validates K8s ServiceAccount tokens and resolves them to agent identities.
#[derive(Clone)]
pub struct SaTokenValidator {
    kube_client: kube::Client,
    audience: String,
}

impl SaTokenValidator {
    /// Create a validator using in-cluster config.
    ///
    /// Returns `None` if not running inside a K8s cluster (e.g. local dev).
    pub async fn try_from_env(audience: impl Into<String>) -> Option<Self> {
        let client = kube::Client::try_default().await.ok()?;
        Some(Self {
            kube_client: client,
            audience: audience.into(),
        })
    }

    /// Validate a bearer token as a K8s SA token.
    ///
    /// On success, returns the `AgentId` parsed from the bound pod name
    /// (format: `agent-{id_prefix}`).
    #[instrument(name = "web.auth.sa_token.validate", skip_all)]
    pub async fn validate(&self, raw_token: &str) -> Result<AgentId, AuthError> {
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
        parse_agent_id_from_pod_name(&pod_name)
    }
}

/// Extract the pod name from TokenReview status extra claims.
///
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

/// Parse agent ID from a deterministic pod name (format: `agent-{id[..8]}`).
///
/// Sandbox pods use `agent-{first 8 chars of agent UUID}` as their name.
/// We search for the agent by ID prefix via the agent repo.
fn parse_agent_id_from_pod_name(pod_name: &str) -> Result<AgentId, AuthError> {
    let prefix = pod_name
        .strip_prefix("agent-")
        .ok_or(AuthError::InvalidToken)?;

    // The pod name contains only the first 8 chars of the UUID.
    // We need to find the full agent by prefix. For now, we construct a UUID
    // by zero-padding the prefix — the caller will look up the actual agent.
    //
    // Agent sandbox_name() produces: agent-{id[..8]}
    // where id is a UUID like "abc12345-6789-..."
    // So prefix is "abc12345" — the first 8 hex chars of the UUID.
    if prefix.len() < 8 {
        return Err(AuthError::InvalidToken);
    }

    // Reconstruct a plausible UUID from the 8-char prefix for lookup.
    // The agent repo will do a prefix-match or exact-match.
    let padded = format!("{prefix}-0000-0000-0000-000000000000");
    let uuid = uuid::Uuid::parse_str(&padded).map_err(|_| AuthError::InvalidToken)?;
    Ok(AgentId::from(uuid))
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

    #[test]
    fn parse_pod_name_extracts_agent_id() {
        let result = parse_agent_id_from_pod_name("agent-abc12345");
        assert!(result.is_ok());
    }

    #[test]
    fn parse_pod_name_rejects_invalid() {
        assert!(parse_agent_id_from_pod_name("not-an-agent").is_err());
        assert!(parse_agent_id_from_pod_name("agent-ab").is_err());
    }
}
