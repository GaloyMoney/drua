use std::collections::{HashMap, HashSet};

use rmcp::{
    model::{CallToolRequestParams, Tool},
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
    pub(crate) read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RegisteredToolSet {
    pub(crate) name: String,
    pub(crate) prefix: String,
    pub(crate) category: String,
    pub(crate) category_description: String,
    pub(crate) tools: Vec<serde_json::Value>,
}

pub(crate) struct McpClient {
    service: RunningService<RoleClient, ()>,
    allowed_tools: Option<HashSet<String>>,
}

pub(crate) type McpClients = HashMap<String, McpClient>;

pub(crate) fn parse_upstreams(
    raw: &str,
    read_only_raw: &str,
) -> anyhow::Result<Vec<UpstreamConfig>> {
    let read_only = read_only_raw
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect::<HashSet<_>>();
    let upstreams = raw
        .split(',')
        .filter_map(|pair| {
            let (name, url) = pair.split_once('=')?;
            let name = name.trim().to_string();
            Some(UpstreamConfig {
                read_only: read_only.contains(&name),
                name,
                url: url.trim().to_string(),
            })
        })
        .collect::<Vec<_>>();
    let configured_names = upstreams
        .iter()
        .map(|upstream| upstream.name.as_str())
        .collect::<HashSet<_>>();
    if let Some(unknown) = read_only
        .iter()
        .find(|name| !configured_names.contains(name.as_str()))
    {
        anyhow::bail!("read-only upstream is not configured: {unknown}");
    }

    Ok(upstreams)
}

pub(crate) async fn discover_upstream(
    upstream: &UpstreamConfig,
    deployment_id: &str,
) -> anyhow::Result<(String, McpClient, RegisteredToolSet)> {
    tracing::info!(name = %upstream.name, url = %upstream.url, "connecting to local MCP server");

    let client = connect_upstream(upstream).await?;
    let registration = discover_tools(upstream, deployment_id, &client).await?;
    let allowed_tools = upstream.read_only.then(|| {
        registration
            .tools
            .iter()
            .filter_map(|tool| tool.get("name").and_then(serde_json::Value::as_str))
            .map(str::to_string)
            .collect()
    });

    Ok((
        upstream.name.clone(),
        McpClient {
            service: client,
            allowed_tools,
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
    clients: &McpClients,
    upstream: &str,
    tool_name: &str,
    arguments: Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let client = clients
        .get(upstream)
        .ok_or_else(|| format!("unknown upstream: {upstream}"))?;

    ensure_tool_allowed(client.allowed_tools.as_ref(), tool_name)?;

    let mut params = CallToolRequestParams::new(tool_name.to_string());
    if let Some(args) = arguments {
        params = params.with_arguments(args);
    }

    let result = client
        .service
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
        .filter(|tool| tool_is_visible(upstream, tool))
        .filter_map(|tool| serde_json::to_value(tool).ok())
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

fn ensure_tool_allowed(
    allowed_tools: Option<&HashSet<String>>,
    tool_name: &str,
) -> Result<(), String> {
    if allowed_tools.is_some_and(|allowed| !allowed.contains(tool_name)) {
        return Err(format!("tool is not read-only: {tool_name}"));
    }
    Ok(())
}

fn tool_is_visible(upstream: &UpstreamConfig, tool: &Tool) -> bool {
    !upstream.read_only
        || tool.annotations.as_ref().is_some_and(|annotations| {
            annotations.read_only_hint == Some(true) && annotations.destructive_hint == Some(false)
        })
}

#[cfg(test)]
mod tests {
    use rmcp::model::ToolAnnotations;

    use super::*;

    #[test]
    fn read_only_upstreams_must_be_configured() {
        let upstreams =
            parse_upstreams("one=http://one.local/mcp,two=http://two.local/mcp", "two").unwrap();

        assert!(!upstreams[0].read_only);
        assert!(upstreams[1].read_only);
        assert!(parse_upstreams("one=http://one.local/mcp", "two").is_err());
    }

    #[test]
    fn read_only_upstreams_require_explicit_safe_annotations() {
        let upstream = UpstreamConfig {
            name: "clickstack".to_string(),
            url: "http://clickstack.local/mcp".to_string(),
            read_only: true,
        };
        let mut tool = Tool::default();
        assert!(!tool_is_visible(&upstream, &tool));

        let mut annotations = ToolAnnotations::new();
        annotations.read_only_hint = Some(true);
        annotations.destructive_hint = Some(false);
        tool.annotations = Some(annotations);
        assert!(tool_is_visible(&upstream, &tool));

        tool.annotations.as_mut().unwrap().destructive_hint = Some(true);
        assert!(!tool_is_visible(&upstream, &tool));
    }

    #[test]
    fn read_only_execution_rejects_tools_outside_the_registered_catalog() {
        let allowed = HashSet::from(["clickstack_search".to_string()]);
        assert!(ensure_tool_allowed(Some(&allowed), "clickstack_search").is_ok());
        assert!(ensure_tool_allowed(Some(&allowed), "clickstack_save_dashboard").is_err());
        assert!(ensure_tool_allowed(None, "unrestricted_tool").is_ok());
    }
}
