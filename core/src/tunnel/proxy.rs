//! `ProxyTunnelToolSet` — installed by `spawn_tunnel_registry_listener`
//! on every replica that does *not* own a given deployment's WebSocket.
//! Mirrors the catalog from the `tunnel_registrations` row so
//! `search_tools` and `describe_tool` answer locally; routes `call`
//! through `POST /internal/tunnel/:deployment_id/call` to the owning pod.

use std::sync::Arc;

use rmcp::model::{CallToolResult, JsonObject, Tool};
use serde::{Deserialize, Serialize};

use crate::auth::AuthSubject;
use crate::toolset::{SearchableToolSet, ToolSetEntry, ToolSetScope, ToolSetsError, TunnelKind};

use super::RegisteredToolSet;

/// Auth presented on the proxy hop. SA token is preferred in cluster;
/// the shared secret is a local-dev / config-driven fallback.
#[derive(Clone)]
pub enum InternalAuth {
    /// `Authorization: Bearer <token>` — TokenReview validated by the
    /// receiver. Token is read from `/var/run/secrets/.../token` once
    /// per call (kubelet rotates the file in place).
    SaToken { token_path: std::path::PathBuf },
    /// `Authorization: Bearer <secret>` — fixed env-loaded value.
    SharedSecret { secret: String },
    /// Local single-replica mode: no internal calls expected.
    Disabled,
}

impl InternalAuth {
    pub async fn header_value(&self) -> Result<String, ToolSetsError> {
        match self {
            InternalAuth::SaToken { token_path } => {
                let raw = tokio::fs::read_to_string(token_path).await.map_err(|e| {
                    ToolSetsError::Tunnel(format!(
                        "read SA token from {}: {e}",
                        token_path.display()
                    ))
                })?;
                Ok(format!("Bearer {}", raw.trim()))
            }
            InternalAuth::SharedSecret { secret } => Ok(format!("Bearer {secret}")),
            InternalAuth::Disabled => Err(ToolSetsError::Tunnel(
                "internal auth disabled — cross-pod tunnel calls unavailable".to_string(),
            )),
        }
    }
}

/// Wire shape of the internal proxy POST body. Matches the receiver
/// in `server::internal_routes`.
#[derive(Serialize, Deserialize)]
pub struct InternalCallReq<'a> {
    pub upstream: &'a str,
    pub tool_name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<JsonObject>,
}

pub struct ProxyTunnelToolSet {
    name: String,
    prefix: String,
    category: String,
    category_description: String,
    upstream_name: String,
    tools: Vec<ToolSetEntry>,
    deployment_id: String,
    /// Session ID this proxy was built against. Sent to the receiver
    /// so it can fence stale calls during a takeover: if the owning
    /// pod's local session has been displaced (different session_id)
    /// the receiver returns 410 Gone instead of dispatching through
    /// the now-stale handle.
    session_id: uuid::Uuid,
    owner_pod_addr: String,
    http: Arc<reqwest::Client>,
    auth: Arc<InternalAuth>,
    scope: ToolSetScope,
}

impl ProxyTunnelToolSet {
    /// Build all `ProxyTunnelToolSet`s for one row of `tunnel_registrations`.
    /// Mirrors the naming logic from `LocalTunnelToolSet::new` so the same
    /// `name`/`prefix` is exposed regardless of which pod the request lands on.
    pub fn build(
        deployment_id: &str,
        session_id: uuid::Uuid,
        owner_pod_addr: &str,
        registrations: &[RegisteredToolSet],
        http: Arc<reqwest::Client>,
        auth: Arc<InternalAuth>,
    ) -> Vec<Self> {
        registrations
            .iter()
            .map(|reg| {
                let tools: Vec<ToolSetEntry> = reg
                    .tools
                    .iter()
                    .filter_map(|t| {
                        let tool: Tool = serde_json::from_value(t.clone()).ok()?;
                        Some(ToolSetEntry {
                            name: tool.name.to_string(),
                            description: tool,
                        })
                    })
                    .collect();

                let name = format!("{}_{}", deployment_id, reg.name).replace('-', "_");
                let prefix = format!("{}_{}", deployment_id, reg.prefix).replace('-', "_");

                Self {
                    name,
                    prefix,
                    category: reg.category.clone(),
                    category_description: reg.category_description.clone(),
                    upstream_name: reg.name.clone(),
                    tools,
                    deployment_id: deployment_id.to_string(),
                    session_id,
                    owner_pod_addr: owner_pod_addr.to_string(),
                    http: http.clone(),
                    auth: auth.clone(),
                    scope: ToolSetScope::Tunnel {
                        deployment_id: deployment_id.to_string(),
                        session_id,
                        kind: TunnelKind::Proxy,
                    },
                }
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl SearchableToolSet for ProxyTunnelToolSet {
    fn name(&self) -> &str {
        &self.name
    }
    fn prefix(&self) -> &str {
        &self.prefix
    }
    fn category(&self) -> &str {
        &self.category
    }
    fn category_description(&self) -> &str {
        &self.category_description
    }
    fn tools(&self) -> &[ToolSetEntry] {
        &self.tools
    }
    fn scope(&self) -> Option<&ToolSetScope> {
        Some(&self.scope)
    }

    async fn call(
        &self,
        _subject: &AuthSubject,
        tool_name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        // session_id in the query so the receiver can fence stale
        // proxies that were built against a now-displaced WS session.
        let url = format!(
            "http://{}/internal/tunnel/{}/call?session_id={}",
            self.owner_pod_addr, self.deployment_id, self.session_id
        );
        let body = InternalCallReq {
            upstream: &self.upstream_name,
            tool_name,
            arguments,
        };
        let auth_header = self.auth.header_value().await?;

        // Slightly above the in-process call_tool 120s timeout so the
        // peer pod's TunnelHandle returns its own timeout error before
        // ours fires (clearer error attribution).
        let resp = self
            .http
            .post(&url)
            .header(reqwest::header::AUTHORIZATION, auth_header)
            .json(&body)
            .timeout(std::time::Duration::from_secs(130))
            .send()
            .await
            .map_err(|e| ToolSetsError::Tunnel(format!("proxy POST {url}: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ToolSetsError::Tunnel(format!(
                "proxy POST {url}: status {status}: {body}"
            )));
        }

        // Response is the raw `CallToolResult` JSON; the peer pod's
        // handler forwards through with no envelope.
        let result: CallToolResult = resp
            .json()
            .await
            .map_err(|e| ToolSetsError::Tunnel(format!("proxy POST decode: {e}")))?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registration(name: &str) -> RegisteredToolSet {
        RegisteredToolSet {
            name: name.to_string(),
            prefix: name.to_string(),
            category: "infrastructure".to_string(),
            category_description: "kube".to_string(),
            tools: vec![],
        }
    }

    #[tokio::test]
    async fn header_value_shared_secret() {
        let auth = InternalAuth::SharedSecret {
            secret: "shh".to_string(),
        };
        assert_eq!(auth.header_value().await.unwrap(), "Bearer shh");
    }

    #[tokio::test]
    async fn header_value_disabled_errors() {
        let auth = InternalAuth::Disabled;
        assert!(matches!(
            auth.header_value().await,
            Err(ToolSetsError::Tunnel(_))
        ));
    }

    /// Sandboxed builds (e.g. nix flake check) have no system CA store
    /// and the default `Client::new()` panics. Skip on that path —
    /// these cases run normally in the dev shell and CI's nextest.
    fn try_test_client() -> Option<reqwest::Client> {
        reqwest::Client::builder().build().ok()
    }

    #[tokio::test]
    async fn proxy_call_url_includes_session_id() {
        let Some(http) = try_test_client() else {
            return;
        };
        let http = Arc::new(http);
        let auth = Arc::new(InternalAuth::SharedSecret {
            secret: "x".to_string(),
        });
        let session = uuid::Uuid::new_v4();
        let proxies = ProxyTunnelToolSet::build(
            "galoy-staging",
            session,
            "127.0.0.1:1",
            &[registration("kubernetes")],
            http,
            auth,
        );
        // Indirect: trigger a call against an unreachable addr; the
        // returned error message must mention `session_id=` so we know
        // the URL was built with the fence parameter.
        let err = proxies[0]
            .call(&AuthSubject::Anonymous, "list_pods", None)
            .await
            .expect_err("unreachable");
        let msg = err.to_string();
        assert!(
            msg.contains(&format!("session_id={session}")),
            "session_id must be in URL: {msg}"
        );
    }

    #[test]
    fn build_one_proxy_per_registration_matching_local_naming() {
        let Some(http) = try_test_client() else {
            return;
        };
        let http = Arc::new(http);
        let auth = Arc::new(InternalAuth::SharedSecret {
            secret: "x".to_string(),
        });
        let regs = vec![registration("kubernetes"), registration("postgres")];
        let proxies = ProxyTunnelToolSet::build(
            "galoy-staging",
            uuid::Uuid::new_v4(),
            "10.0.0.1:4200",
            &regs,
            http,
            auth,
        );
        assert_eq!(proxies.len(), 2);
        // Same name shape as LocalTunnelToolSet — peer pods can't tell
        // proxy from local, which is the point.
        assert_eq!(proxies[0].name(), "galoy_staging_kubernetes");
        assert_eq!(proxies[0].prefix(), "galoy_staging_kubernetes");
        assert!(matches!(
            proxies[0].scope(),
            Some(ToolSetScope::Tunnel {
                kind: TunnelKind::Proxy,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn proxy_call_unreachable_owner_surfaces_tunnel_error() {
        let Some(http) = try_test_client() else {
            return;
        };
        let http = Arc::new(http);
        let auth = Arc::new(InternalAuth::SharedSecret {
            secret: "x".to_string(),
        });
        // Loopback port 1 is reliably unreachable for a TCP connect.
        let proxies = ProxyTunnelToolSet::build(
            "galoy-staging",
            uuid::Uuid::new_v4(),
            "127.0.0.1:1",
            &[registration("kubernetes")],
            http,
            auth,
        );
        let result = proxies[0]
            .call(&AuthSubject::Anonymous, "list_pods", None)
            .await;
        assert!(matches!(result, Err(ToolSetsError::Tunnel(_))));
    }
}
