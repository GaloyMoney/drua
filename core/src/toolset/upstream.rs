use std::collections::HashMap;

use http::{HeaderName, HeaderValue};
use rmcp::{
    model::{CallToolRequestParams, CallToolResult, JsonObject, Tool},
    service::RunningService,
    transport::streamable_http_client::{
        StreamableHttpClientTransportConfig, StreamableHttpClientWorker,
    },
    Peer, RoleClient, ServiceExt,
};

use super::{McpUpstreamConfig, ToolSetsError};

pub struct UpstreamToolSet {
    pub name: String,
    pub url: String,
    pub category: Option<String>,
    pub category_description: Option<String>,
    pub tools: Vec<UpstreamTool>,
    client: RunningService<RoleClient, ()>,
}

pub struct UpstreamTool {
    pub description: Tool,
}

impl UpstreamToolSet {
    pub(super) async fn init(
        upstream: &McpUpstreamConfig,
    ) -> Result<UpstreamToolSet, ToolSetsError> {
        let mut headers = HashMap::new();
        headers.insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_str(&upstream.auth_header)
                .map_err(|e| ToolSetsError::InvalidHeader(e.to_string()))?,
        );

        let transport_config = StreamableHttpClientTransportConfig::with_uri(upstream.url.as_str())
            .custom_headers(headers);

        let worker = StreamableHttpClientWorker::new(reqwest::Client::new(), transport_config);

        let client = ().serve(worker).await?;

        let tools = client
            .list_all_tools()
            .await?
            .into_iter()
            .map(|description| UpstreamTool { description })
            .collect();

        Ok(UpstreamToolSet {
            name: upstream.name.clone(),
            url: upstream.url.clone(),
            category: upstream.category.clone(),
            category_description: upstream.category_description.clone(),
            tools,
            client,
        })
    }

    pub fn peer(&self) -> &Peer<RoleClient> {
        self.client.peer()
    }

    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let mut params = CallToolRequestParams::new(name.to_string());
        if let Some(args) = arguments {
            params = params.with_arguments(args);
        }
        let result = self.peer().call_tool(params).await?;
        Ok(result)
    }
}
