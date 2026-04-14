use std::collections::HashMap;

use http::{HeaderName, HeaderValue};
use rmcp::{
    model::{CallToolRequestParams, CallToolResult, JsonObject},
    service::RunningService,
    transport::streamable_http_client::{
        StreamableHttpClientTransportConfig, StreamableHttpClientWorker,
    },
    Peer, RoleClient, ServiceExt,
};

use crate::auth::AuthSubject;

use super::super::{McpUpstreamConfig, SearchableToolSet, ToolSetEntry, ToolSetsError};

pub struct UpstreamToolSet {
    name: String,
    tool_prefix: String,
    category: String,
    category_description: String,
    /// Scopes required to see / invoke this upstream. Empty = unrestricted.
    required_scopes: Vec<String>,
    tools: Vec<ToolSetEntry>,
    client: RunningService<RoleClient, ()>,
}

impl UpstreamToolSet {
    pub(in super::super) async fn init(
        upstream: &McpUpstreamConfig,
    ) -> Result<UpstreamToolSet, ToolSetsError> {
        let mut headers = HashMap::new();
        if !upstream.auth_header.is_empty() {
            headers.insert(
                HeaderName::from_bytes(upstream.auth_header_name.as_bytes())
                    .map_err(|e| ToolSetsError::InvalidHeader(e.to_string()))?,
                HeaderValue::from_str(&upstream.auth_header)
                    .map_err(|e| ToolSetsError::InvalidHeader(e.to_string()))?,
            );
        }

        let transport_config = StreamableHttpClientTransportConfig::with_uri(upstream.url.as_str())
            .custom_headers(headers);

        let worker = StreamableHttpClientWorker::new(reqwest::Client::new(), transport_config);

        let client = ().serve(worker).await.map_err(Box::new)?;

        let allowed = upstream.allowed_tools.as_ref();
        let tools: Vec<ToolSetEntry> = client
            .list_all_tools()
            .await?
            .into_iter()
            .filter(|t| {
                allowed
                    .map(|list| list.iter().any(|a| a == t.name.as_ref()))
                    .unwrap_or(true)
            })
            .map(|description| ToolSetEntry {
                name: description.name.to_string(),
                description,
                default_output_filter: None,
            })
            .collect();

        let tool_prefix = upstream
            .tool_prefix
            .clone()
            .unwrap_or_else(|| upstream.name.clone());

        Ok(UpstreamToolSet {
            name: upstream.name.clone(),
            tool_prefix,
            category: upstream.category.clone().unwrap_or_default(),
            category_description: upstream.category_description.clone().unwrap_or_default(),
            required_scopes: upstream
                .required_scopes
                .clone()
                .unwrap_or_default(),
            tools,
            client,
        })
    }

    fn peer(&self) -> &Peer<RoleClient> {
        self.client.peer()
    }
}

#[async_trait::async_trait]
impl SearchableToolSet for UpstreamToolSet {
    fn name(&self) -> &str {
        &self.name
    }

    fn prefix(&self) -> &str {
        &self.tool_prefix
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

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        has_required_scopes(&self.required_scopes, subject)
    }

    fn can_execute(&self, subject: &AuthSubject) -> bool {
        has_required_scopes(&self.required_scopes, subject)
    }

    async fn call(
        &self,
        tool_name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let mut params = CallToolRequestParams::new(tool_name.to_string());
        if let Some(args) = arguments {
            params = params.with_arguments(args);
        }
        let result = self.peer().call_tool(params).await?;
        Ok(result)
    }
}

fn has_required_scopes(required: &[String], subject: &AuthSubject) -> bool {
    required.iter().all(|scope| subject.has_scope(scope))
}
