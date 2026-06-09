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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UpstreamConfig {
    pub(crate) name: String,
    pub(crate) url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RegisteredToolSet {
    pub(crate) name: String,
    pub(crate) prefix: String,
    pub(crate) category: String,
    pub(crate) category_description: String,
    pub(crate) tools: Vec<serde_json::Value>,
}

pub(crate) type McpClients = HashMap<String, RunningService<RoleClient, ()>>;

pub(crate) fn parse_upstreams(raw: &str) -> Vec<UpstreamConfig> {
    raw.split(',')
        .filter_map(|pair| {
            let (name, url) = pair.split_once('=')?;
            Some(UpstreamConfig {
                name: name.trim().to_string(),
                url: url.trim().to_string(),
            })
        })
        .collect()
}

pub(crate) async fn discover_upstream(
    upstream: &UpstreamConfig,
    deployment_id: &str,
) -> anyhow::Result<(String, RunningService<RoleClient, ()>, RegisteredToolSet)> {
    tracing::info!(name = %upstream.name, url = %upstream.url, "connecting to local MCP server");

    let client = connect_upstream(upstream).await?;
    let registration = discover_tools(upstream, deployment_id, &client).await?;

    Ok((upstream.name.clone(), client, registration))
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
    clients: &McpClients,
    upstream: &str,
    tool_name: &str,
    arguments: Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let client = clients
        .get(upstream)
        .ok_or_else(|| format!("unknown upstream: {upstream}"))?;

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
    let config = StreamableHttpClientTransportConfig::with_uri(upstream.url.as_str());
    let worker = StreamableHttpClientWorker::new(reqwest::Client::new(), config);
    Ok(().serve(worker).await?)
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
