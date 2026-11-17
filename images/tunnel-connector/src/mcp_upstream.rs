use std::collections::HashMap;

use rmcp::{
    model::CallToolRequestParams,
    service::RunningService,
    transport::streamable_http_client::{
        StreamableHttpClientTransportConfig, StreamableHttpClientWorker,
    },
    RoleClient, ServiceExt,
};
use serde::{Deserialize, Serialize};

/// Direct-grant (resource-owner password) credentials for an upstream that
/// requires a bearer token, e.g. a per-instance Lana admin MCP endpoint gated
/// by a Keycloak realm. The connector fetches a token at connect time and
/// re-fetches + reconnects on call failure (tokens expire mid-session).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DirectGrantAuth {
    pub(crate) token_url: String,
    pub(crate) client_id: String,
    pub(crate) username: String,
    pub(crate) password: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UpstreamConfig {
    pub(crate) name: String,
    pub(crate) url: String,
    pub(crate) auth: Option<DirectGrantAuth>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RegisteredToolSet {
    pub(crate) name: String,
    pub(crate) prefix: String,
    pub(crate) category: String,
    pub(crate) category_description: String,
    pub(crate) tools: Vec<serde_json::Value>,
}

/// A connected upstream plus the config needed to reconnect it (token refresh
/// on expiry requires a full transport rebuild — the rmcp client header is
/// fixed at construction).
pub(crate) struct UpstreamClient {
    pub(crate) upstream: UpstreamConfig,
    pub(crate) client: RunningService<RoleClient, ()>,
}

pub(crate) type McpClients = HashMap<String, UpstreamClient>;

pub(crate) fn parse_upstreams(raw: &str) -> Vec<UpstreamConfig> {
    raw.split(',')
        .filter_map(|pair| {
            let (name, url) = pair.split_once('=')?;
            Some(UpstreamConfig {
                name: name.trim().to_string(),
                url: url.trim().to_string(),
                auth: None,
            })
        })
        .collect()
}

pub(crate) async fn discover_upstream(
    upstream: &UpstreamConfig,
    deployment_id: &str,
) -> anyhow::Result<(String, UpstreamClient, RegisteredToolSet)> {
    tracing::info!(name = %upstream.name, url = %upstream.url, "connecting to local MCP server");

    let client = connect_upstream(upstream).await?;
    let registration = discover_tools(upstream, deployment_id, &client).await?;

    Ok((
        upstream.name.clone(),
        UpstreamClient {
            upstream: upstream.clone(),
            client,
        },
        registration,
    ))
}

pub(crate) fn registration_fingerprint(
    registrations: &[RegisteredToolSet],
) -> anyhow::Result<String> {
    Ok(serde_json::to_string(registrations)?)
}

pub(crate) async fn tool_catalog_changed(
    upstreams: &[UpstreamConfig],
    deployment_id: &str,
    current_fingerprint: &str,
) -> anyhow::Result<bool> {
    let mut registrations = Vec::with_capacity(upstreams.len());

    for upstream in upstreams {
        let client = match connect_upstream(upstream).await {
            Ok(client) => client,
            Err(e) => {
                tracing::warn!(
                    name = %upstream.name,
                    url = %upstream.url,
                    error = %e,
                    "skipping tool catalog refresh because an upstream is unavailable"
                );
                return Ok(false);
            }
        };
        registrations.push(discover_tools(upstream, deployment_id, &client).await?);
    }

    Ok(registration_fingerprint(&registrations)? != current_fingerprint)
}

pub(crate) async fn call_tool(
    clients: &mut McpClients,
    upstream: &str,
    tool_name: &str,
    arguments: Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    match clients.get(upstream) {
        Some(entry) => match call_tool_on(&entry.client, tool_name, arguments.clone()).await {
            Ok(result) => return Ok(result),
            Err(e) => {
                if entry.upstream.auth.is_none() {
                    return Err(e);
                }
                tracing::warn!(
                    upstream = %upstream,
                    error = %e,
                    "authenticated upstream call failed; refreshing token and retrying once"
                );
            }
        },
        None => return Err(format!("unknown upstream: {upstream}")),
    }

    let entry = clients.get(upstream).expect("checked above");
    let refreshed = connect_upstream(&entry.upstream)
        .await
        .map_err(|e| format!("reconnect {upstream}: {e}"))?;
    let entry = clients.get_mut(upstream).expect("checked above");
    entry.client = refreshed;
    call_tool_on(&entry.client, tool_name, arguments).await
}

async fn call_tool_on(
    client: &RunningService<RoleClient, ()>,
    tool_name: &str,
    arguments: Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let mut params = CallToolRequestParams::new(tool_name.to_string());
    if let Some(args) = arguments {
        params = params.with_arguments(args);
    }

    let result = client
        .peer()
        .call_tool(params)
        .await
        .map_err(|e| e.to_string())?;

    serde_json::to_value(&result).map_err(|e| format!("serialize result: {e}"))
}

async fn connect_upstream(
    upstream: &UpstreamConfig,
) -> anyhow::Result<RunningService<RoleClient, ()>> {
    let mut config = StreamableHttpClientTransportConfig::with_uri(upstream.url.as_str());
    if let Some(auth) = &upstream.auth {
        let token = fetch_direct_grant_token(auth).await?;
        config = config.auth_header(token);
    }
    let worker = StreamableHttpClientWorker::new(reqwest::Client::new(), config);
    Ok(().serve(worker).await?)
}

/// Mint a bearer token via the OAuth2 resource-owner password grant. Used for
/// Lana admin MCP upstreams whose Keycloak realm runs the DEV direct-grant
/// flow (staging sandboxes / kind), where seeded staff users have empty
/// passwords.
pub(crate) async fn fetch_direct_grant_token(auth: &DirectGrantAuth) -> anyhow::Result<String> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "password")
        .append_pair("client_id", &auth.client_id)
        .append_pair("username", &auth.username)
        .append_pair("password", &auth.password)
        .finish();

    let response = reqwest::Client::new()
        .post(&auth.token_url)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(body)
        .send()
        .await?;

    let status = response.status();
    let body: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);
    if !status.is_success() {
        anyhow::bail!(
            "direct-grant token request failed with {status}: {}",
            body.get("error_description")
                .or_else(|| body.get("error"))
                .and_then(|e| e.as_str())
                .unwrap_or("unknown error")
        );
    }

    body.get("access_token")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("direct-grant token response missing access_token"))
}

async fn discover_tools(
    upstream: &UpstreamConfig,
    deployment_id: &str,
    client: &RunningService<RoleClient, ()>,
) -> anyhow::Result<RegisteredToolSet> {
    let tools: Vec<serde_json::Value> = client
        .list_all_tools()
        .await?
        .into_iter()
        .filter_map(|t| serde_json::to_value(t).ok())
        .collect();

    tracing::info!(name = %upstream.name, tools = tools.len(), "discovered tools");

    Ok(RegisteredToolSet {
        name: upstream.name.clone(),
        prefix: upstream.name.clone(),
        category: "deployment".to_string(),
        category_description: format!("{deployment_id} deployment"),
        tools,
    })
}
