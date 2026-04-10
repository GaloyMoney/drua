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
            Some(auth @ AuthContext::ExportedAgent(_, _, _))
            | Some(auth @ AuthContext::Agent(_, _, _)) => Ok(auth),
            _ => Err(ErrorData::new(
                ErrorCode::INVALID_REQUEST,
                "Authentication required: provide a valid Bearer token",
                None::<serde_json::Value>,
            )),
        }
    }

    fn catalog(&self) -> galoy_agents_core::toolset::Catalog {
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
    /// Optional post-processing filter applied to tool output. Reduces output
    /// size to save tokens. By default, output is capped at 1000 lines
    /// (some tools override this, e.g. build logs default to tail 150).
    #[serde(default)]
    #[schemars(schema_with = "output_filter_schema")]
    output_filter: Option<galoy_agents_core::toolset::OutputFilter>,
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

/// Manual schemars 1.x schema for `Option<OutputFilter>`.
///
/// rmcp re-exports schemars 1.x while the workspace uses 0.8, so we cannot
/// derive `JsonSchema` on `OutputFilter` for both versions. This function
/// provides the schema inline so the MCP tool definition exposes
/// `output_filter` to clients.
fn output_filter_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let string_schema = generator.subschema_for::<String>();
    let int_schema = generator.subschema_for::<usize>();
    let bool_schema = generator.subschema_for::<bool>();

    schemars::json_schema!({
        "type": "object",
        "description": "Optional post-processing filter applied to tool output. Reduces output size to save tokens.",
        "properties": {
            "grep": {
                "description": "Regex pattern to filter output lines (only matching lines returned)",
                "allOf": [string_schema]
            },
            "invert_match": {
                "description": "Exclude matching lines instead of including them (grep -v). Default: false",
                "allOf": [bool_schema]
            },
            "context_lines": {
                "description": "Lines of context around grep matches (grep -C). Only used with grep",
                "allOf": [int_schema]
            },
            "head": {
                "description": "Return only the first N lines",
                "allOf": [int_schema]
            },
            "tail": {
                "description": "Return only the last N lines",
                "allOf": [int_schema]
            }
        }
    })
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

        let text = galoy_agents_core::toolset::Catalog::format_search_results(&results);
        Ok(CallToolResult::success(vec![Content::text(text)]))
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

        let text = galoy_agents_core::toolset::Catalog::format_describe(&entry);
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
            .call_with_filter(&params.tool_name, params.arguments, params.output_filter)
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

    #[test]
    fn call_tool_params_accepts_output_filter() {
        let json = serde_json::json!({
            "tool_name": "concourse_get_build_logs",
            "arguments": {"build_id": 123},
            "output_filter": {
                "grep": "error",
                "tail": 50
            }
        });
        let params: CallToolParams = serde_json::from_value(json).unwrap();
        let filter = params.output_filter.unwrap();
        assert_eq!(filter.grep.as_deref(), Some("error"));
        assert_eq!(filter.tail, Some(50));
        assert!(filter.head.is_none());
    }

    #[test]
    fn call_tool_params_accepts_missing_output_filter() {
        let json = serde_json::json!({
            "tool_name": "some_tool"
        });
        let params: CallToolParams = serde_json::from_value(json).unwrap();
        assert!(params.output_filter.is_none());
    }
}
