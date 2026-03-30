use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ErrorCode, ErrorData, ServerCapabilities, ServerInfo};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use rmcp::{schemars, tool, tool_handler, tool_router, ServerHandler};

use code_assistant_server::SearchCodeParams;
use galoy_agents_core::auth::AuthContext;
use galoy_agents_core::App;

pub use code_assistant_server::{self, CodeAssistantConfig, CodeAssistantEndpoints};

#[derive(Clone)]
pub struct McpGateway {
    app: App,
    code_assistant: Option<CodeAssistantEndpoints>,
    tool_router: ToolRouter<Self>,
}

impl McpGateway {
    fn new(app: App, code_assistant: Option<CodeAssistantEndpoints>) -> Self {
        Self {
            app,
            code_assistant,
            tool_router: Self::tool_router(),
        }
    }

    /// Build the MCP service from pre-constructed endpoints.
    pub fn service(
        app: App,
        code_assistant: Option<CodeAssistantEndpoints>,
    ) -> StreamableHttpService<Self, LocalSessionManager> {
        let mut config = StreamableHttpServerConfig::default();
        config.stateful_mode = false;
        config.json_response = true;
        StreamableHttpService::new(
            move || Ok(McpGateway::new(app.clone(), code_assistant.clone())),
            LocalSessionManager::default().into(),
            config,
        )
    }

    fn require_auth(parts: &http::request::Parts) -> Result<(), ErrorData> {
        match parts.extensions.get::<AuthContext>() {
            Some(AuthContext::Agent(_, _)) => Ok(()),
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
    #[serde(default)]
    arguments: Option<serde_json::Map<String, serde_json::Value>>,
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

    /// Search indexed code repositories for patterns and conventions.
    #[tool(
        description = "Search indexed codebases for code patterns matching a query.\n\nUsage tips:\n- Pass a code snippet as the query (e.g. the pattern you are about to write) — code-as-query gives much better results than natural language\n- Always pass a `label` filter for precise results\n- Adopt the style, naming, and structure from the returned examples — don't guess conventions, search first\n\nAvailable labels: entity, entity_event, entity_command, entity_query, entity_hydration, error, service, service_method, repository, domain_primitives, value_object, type_conversion, config, test, api, job, event_handler, authorization, published_event, new_entity, none (unlabeled chunks)\n\nAvailable filters: query (required), limit, repo, language, label"
    )]
    async fn search_code(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(params): Parameters<SearchCodeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Self::require_auth(&parts)?;
        match self.code_assistant.as_ref() {
            Some(agent) => agent.search_code(params).await,
            None => Err(ErrorData::new(
                ErrorCode::INTERNAL_ERROR,
                "Code assistant is disabled (no db_path configured)",
                None::<serde_json::Value>,
            )),
        }
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
        Self::require_auth(&parts)?;
        let catalog = self.catalog();
        let results = catalog.search(params.query.as_deref(), params.category.as_deref());

        if results.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No tools found matching your query.",
            )]));
        }

        let mut lines = Vec::new();
        let mut current_category: Option<&str> = None;
        for entry in &results {
            let cat = entry.category.as_deref().unwrap_or("uncategorized");
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
        Self::require_auth(&parts)?;
        let catalog = self.catalog();

        let entry = catalog.describe(&params.tool_name).ok_or_else(|| {
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
            entry.category.as_deref().unwrap_or("uncategorized"),
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
        Self::require_auth(&parts)?;
        let catalog = self.catalog();

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
