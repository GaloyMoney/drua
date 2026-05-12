use std::sync::Arc;

use rmcp::model::{CallToolResult, JsonObject, Tool};

use crate::auth::AuthSubject;
use crate::toolset::{SearchableToolSet, ToolSetEntry, ToolSetScope, ToolSetsError, TunnelRoute};

use super::{InternalAuth, InternalCallReq, RegisteredToolSet};

pub struct ProxyTunnelToolSet {
    name: String,
    prefix: String,
    category: String,
    category_description: String,
    upstream_name: String,
    tools: Vec<ToolSetEntry>,
    deployment_id: String,
    session_id: uuid::Uuid,
    owner_pod_addr: String,
    http: Arc<reqwest::Client>,
    auth: Arc<InternalAuth>,
    scope: ToolSetScope,
}

impl ProxyTunnelToolSet {
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
                        route: TunnelRoute::Proxy,
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
        let url = format!(
            "http://{}/internal/tunnel/{}/call?session_id={}",
            self.owner_pod_addr, self.deployment_id, self.session_id
        );
        let body = InternalCallReq {
            upstream: &self.upstream_name,
            tool_name,
            arguments,
        };
        let auth_header = self
            .auth
            .header_value()
            .map_err(|e| ToolSetsError::Tunnel(e.to_string()))?;

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

    #[test]
    fn header_value_shared_secret() {
        let auth = InternalAuth::SharedSecret {
            secret: "shh".to_string(),
        };
        assert_eq!(auth.header_value().unwrap(), "Bearer shh");
    }

    #[test]
    fn header_value_disabled_errors() {
        let auth = InternalAuth::Disabled;
        assert!(auth.header_value().is_err());
    }

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
    fn build_one_proxy_per_registration_matching_owned_naming() {
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
        assert_eq!(proxies[0].name(), "galoy_staging_kubernetes");
        assert_eq!(proxies[0].prefix(), "galoy_staging_kubernetes");
        assert!(matches!(
            proxies[0].scope(),
            Some(ToolSetScope::Tunnel {
                route: TunnelRoute::Proxy,
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
