use std::sync::Arc;

use rmcp::{
    model::{CallToolRequestParams, CallToolResult, JsonObject},
    service::RunningService,
    transport::streamable_http_client::{
        StreamableHttpClientTransportConfig, StreamableHttpClientWorker,
    },
    Peer, RoleClient, ServiceExt,
};

use crate::auth::AuthSubject;
use crate::mcp_jwt::McpJwtSigner;
use crate::primitives::AuthScope;

use super::super::{McpUpstreamConfig, SearchableToolSet, ToolSetEntry, ToolSetsError};
use super::jwt_http_client::JwtSigningHttpClient;

/// MCP upstream that lives on a remote deployment (e.g. the
/// `galoy-agents-proxy` sidecar). Every outbound HTTP request is
/// authenticated with a fresh short-lived RS256 JWT signed by the
/// shared `McpJwtSigner` — JWT minting happens inside
/// [`JwtSigningHttpClient`] on each `post_message` / `get_stream` /
/// `delete_session`, so there's no "pod uptime > TTL → silent 401"
/// failure mode. Remote Envoys validate via `/.well-known/jwks.json`.
///
/// Progressive disclosure (`search_tools` / `describe_tool` /
/// `call_tool`) works exactly like `UpstreamToolSet` — the tool catalog
/// is fetched at init via MCP `tools/list` and exposed through
/// `SearchableToolSet`.
pub struct RemoteProxyToolSet {
    name: String,
    tool_prefix: String,
    category: String,
    category_description: String,
    required_scopes: Vec<AuthScope>,
    tools: Vec<ToolSetEntry>,
    client: RunningService<RoleClient, ()>,
}

impl RemoteProxyToolSet {
    pub(in super::super) async fn init(
        upstream: &McpUpstreamConfig,
        audience: &str,
        signer: &McpJwtSigner,
    ) -> Result<Self, ToolSetsError> {
        let http_client = JwtSigningHttpClient::new(Arc::new(signer.clone()), audience.to_string());

        let transport_config =
            StreamableHttpClientTransportConfig::with_uri(upstream.url.as_str());
        let worker = StreamableHttpClientWorker::new(http_client, transport_config);
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

        tracing::info!(
            name = %upstream.name,
            audience = %audience,
            tool_count = tools.len(),
            "RemoteProxy MCP upstream initialized"
        );

        Ok(Self {
            name: upstream.name.clone(),
            tool_prefix,
            category: upstream.category.clone().unwrap_or_default(),
            category_description: upstream.category_description.clone().unwrap_or_default(),
            required_scopes: upstream.required_scopes.clone().unwrap_or_default(),
            tools,
            client,
        })
    }

    fn peer(&self) -> &Peer<RoleClient> {
        self.client.peer()
    }
}

#[async_trait::async_trait]
impl SearchableToolSet for RemoteProxyToolSet {
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
        self.required_scopes.iter().all(|s| subject.has_scope(s))
    }

    fn can_execute(&self, subject: &AuthSubject) -> bool {
        self.required_scopes.iter().all(|s| subject.has_scope(s))
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
