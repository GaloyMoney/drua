use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ErrorCode, ErrorData, ServerCapabilities, ServerInfo};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use rmcp::{schemars, tool, tool_handler, tool_router, ServerHandler};

use galoy_agents_core::auth::AuthContext;
use galoy_agents_core::App;

#[derive(Clone)]
pub struct McpGateway {
    app: App,
    tool_router: ToolRouter<Self>,
}

impl McpGateway {
    fn new(app: App) -> Self {
        Self {
            app,
            tool_router: Self::tool_router(),
        }
    }

    /// Build the MCP service.
    pub fn service(app: App) -> StreamableHttpService<Self, LocalSessionManager> {
        let mut config = StreamableHttpServerConfig::default();
        config.stateful_mode = false;
        config.json_response = true;
        StreamableHttpService::new(
            move || Ok(McpGateway::new(app.clone())),
            LocalSessionManager::default().into(),
            config,
        )
    }

    fn require_auth(parts: &http::request::Parts) -> Result<&AuthContext, ErrorData> {
        match parts.extensions.get::<AuthContext>() {
            Some(auth @ AuthContext::McpCreds(_, _)) => Ok(auth),
            _ => Err(ErrorData::new(
                ErrorCode::INVALID_REQUEST,
                "Authentication required: provide a valid Bearer token",
                None::<serde_json::Value>,
            )),
        }
    }

    fn catalog(&self) -> &galoy_agents_core::toolset::Catalog {
        self.app.toolsets().catalog()
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct HelloParams {
    name: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SearchToolsParams {
    /// Search query (e.g., 'pipeline status', 'customer accounts', 'code review')
    #[serde(default)]
    query: Option<String>,
    /// Filter by service category (e.g., 'ci', 'observability', 'banking', 'code-quality', or 'all')
    #[serde(default)]
    category: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct DescribeToolParams {
    /// The tool name returned from search_tools (e.g., 'honeycomb_list_environments')
    #[serde(alias = "name")]
    tool_name: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct CallToolParams {
    /// The prefixed tool name (e.g., 'honeycomb_list_environments')
    #[serde(alias = "name")]
    tool_name: String,
    /// Tool arguments matching the schema from describe_tool
    #[serde(default, deserialize_with = "deserialize_arguments")]
    arguments: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Deserialize `arguments` from either a JSON object or a stringified JSON object.
///
/// Some MCP clients send arguments as a JSON string (e.g. `"{\"key\": \"value\"}"`)
/// instead of a parsed object (`{"key": "value"}`). This deserializer accepts both.
fn deserialize_arguments<'de, D>(
    deserializer: D,
) -> Result<Option<serde_json::Map<String, serde_json::Value>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    use serde::Deserialize;

    let value: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Object(map)) => Ok(Some(map)),
        Some(serde_json::Value::String(s)) => {
            let parsed: serde_json::Value = serde_json::from_str(&s)
                .map_err(|e| D::Error::custom(format!("invalid JSON in arguments string: {e}")))?;
            match parsed {
                serde_json::Value::Object(map) => Ok(Some(map)),
                _ => Err(D::Error::custom(
                    "arguments string must contain a JSON object",
                )),
            }
        }
        Some(_) => Err(D::Error::custom(
            "arguments must be a JSON object or a JSON string containing an object",
        )),
    }
}

#[tool_router]
impl McpGateway {
    #[tool(description = "Say hello")]
    async fn hello(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(params): Parameters<HelloParams>,
    ) -> Result<String, ErrorData> {
        Self::require_auth(&parts)?;
        Ok(format!("Hello, {}!", params.name))
    }

    /// Search for available tools across all upstream services.
    #[tool(
        description = "Search for available tools across all upstream MCP services. Returns tool names, brief descriptions, and categories. Use this first to find relevant tools before calling them.\n\nTip: Use describe_tool to get full parameter schemas before calling a tool."
    )]
    async fn search_tools(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(params): Parameters<SearchToolsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let auth = Self::require_auth(&parts)?;
        let catalog = self.catalog().with_auth(auth);
        let results = catalog
            .search(params.query.as_deref(), params.category.as_deref())
            .await;

        if results.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No tools found matching your query.",
            )]));
        }

        let mut lines = Vec::new();
        let mut current_category: Option<&str> = None;
        for entry in &results {
            let cat = entry.category.as_str();
            if current_category != Some(cat) {
                if !lines.is_empty() {
                    lines.push(String::new());
                }
                lines.push(format!("{}:", cat));
                current_category = Some(cat);
            }
            lines.push(format!(
                "  {:40} - {}",
                entry.prefixed_name, entry.brief_description
            ));
        }

        Ok(CallToolResult::success(vec![Content::text(
            lines.join("\n"),
        )]))
    }

    /// Get the full parameter schema and detailed description for a specific tool.
    #[tool(
        description = "Get the full parameter schema and detailed description for a specific tool. Use after search_tools to understand how to call a tool."
    )]
    async fn describe_tool(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(params): Parameters<DescribeToolParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let auth = Self::require_auth(&parts)?;
        let catalog = self.catalog().with_auth(auth);

        let entry = catalog.describe(&params.tool_name).await.ok_or_else(|| {
            ErrorData::new(
                ErrorCode::INVALID_PARAMS,
                format!(
                    "Tool '{}' not found. Use search_tools to find available tools.",
                    params.tool_name
                ),
                None::<serde_json::Value>,
            )
        })?;

        let tool = &entry.full_tool;
        let description = tool
            .description
            .as_deref()
            .unwrap_or("No description available.");
        let schema =
            serde_json::to_string_pretty(&tool.input_schema).unwrap_or_else(|_| "{}".to_string());

        let text = format!(
            "## {}\n\nUpstream: {}\nCategory: {}\n\n{}\n\n### Parameters\n```json\n{}\n```\n\nUse call_tool(\"{}\", {{...}}) to execute.",
            entry.prefixed_name,
            entry.upstream_name,
            entry.category,
            description,
            schema,
            entry.prefixed_name,
        );

        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// Execute an upstream tool by name with the provided arguments.
    #[tool(
        description = "Execute an upstream tool by name with the provided arguments. Use describe_tool first to understand the required parameters."
    )]
    async fn call_tool(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(params): Parameters<CallToolParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let auth = Self::require_auth(&parts)?;
        let catalog = self.catalog().with_auth(auth);

        catalog
            .call(&params.tool_name, params.arguments)
            .await
            .map_err(|e| {
                ErrorData::new(
                    ErrorCode::INTERNAL_ERROR,
                    e.to_string(),
                    None::<serde_json::Value>,
                )
            })
    }
}

#[tool_handler]
impl ServerHandler for McpGateway {
    fn get_info(&self) -> ServerInfo {
        let instructions = format!(
            "Galoy Agents MCP Gateway\n\n{}",
            self.catalog().instructions()
        );
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(instructions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_tool_params_accepts_object_arguments() {
        let json = serde_json::json!({
            "tool_name": "honeycomb_query",
            "arguments": {"query": "test", "label": "service_method"}
        });
        let params: CallToolParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.tool_name, "honeycomb_query");
        let args = params.arguments.unwrap();
        assert_eq!(args["query"], "test");
        assert_eq!(args["label"], "service_method");
    }

    #[test]
    fn call_tool_params_accepts_stringified_arguments() {
        let json = serde_json::json!({
            "tool_name": "search_code",
            "arguments": "{\n  \"query\": \"create_in_op DbOp atomic\",\n  \"label\": \"service_method\"\n}"
        });
        let params: CallToolParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.tool_name, "search_code");
        let args = params.arguments.unwrap();
        assert_eq!(args["query"], "create_in_op DbOp atomic");
        assert_eq!(args["label"], "service_method");
    }

    #[test]
    fn call_tool_params_accepts_null_arguments() {
        let json = serde_json::json!({
            "tool_name": "some_tool",
            "arguments": null
        });
        let params: CallToolParams = serde_json::from_value(json).unwrap();
        assert!(params.arguments.is_none());
    }

    #[test]
    fn call_tool_params_accepts_missing_arguments() {
        let json = serde_json::json!({
            "tool_name": "some_tool"
        });
        let params: CallToolParams = serde_json::from_value(json).unwrap();
        assert!(params.arguments.is_none());
    }

    #[test]
    fn call_tool_params_rejects_invalid_json_string() {
        let json = serde_json::json!({
            "tool_name": "some_tool",
            "arguments": "not valid json"
        });
        let result = serde_json::from_value::<CallToolParams>(json);
        assert!(result.is_err());
    }

    #[test]
    fn call_tool_params_rejects_non_object_json_string() {
        let json = serde_json::json!({
            "tool_name": "some_tool",
            "arguments": "[1, 2, 3]"
        });
        let result = serde_json::from_value::<CallToolParams>(json);
        assert!(result.is_err());
    }
}
