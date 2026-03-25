use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ErrorCode, ErrorData, ServerCapabilities, ServerInfo};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use rmcp::{schemars, tool, tool_handler, tool_router, ServerHandler};

use galoy_agents_domain::auth::AuthContext;
use galoy_agents_domain::App;
use style_agent_server::{SearchCodeParams, StyleAgentEndpoints};

pub use style_agent_server::StyleAgentConfig;

#[derive(Clone)]
pub struct McpGateway {
    #[allow(dead_code)]
    app: App,
    style_agent: Option<StyleAgentEndpoints>,
    tool_router: ToolRouter<Self>,
}

impl McpGateway {
    fn new(app: App, style_agent: Option<StyleAgentEndpoints>) -> Self {
        Self {
            app,
            style_agent,
            tool_router: Self::tool_router(),
        }
    }

    pub fn service(
        app: App,
        style_agent_config: &StyleAgentConfig,
    ) -> anyhow::Result<StreamableHttpService<Self, LocalSessionManager>> {
        let style_agent = style_agent_server::init_endpoints(style_agent_config)?;

        let config = StreamableHttpServerConfig {
            stateful_mode: false,
            json_response: true,
            ..Default::default()
        };
        Ok(StreamableHttpService::new(
            move || Ok(McpGateway::new(app.clone(), style_agent.clone())),
            LocalSessionManager::default().into(),
            config,
        ))
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
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct HelloParams {
    name: String,
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
        match self.style_agent.as_ref() {
            Some(agent) => agent.search_code(params).await,
            None => Err(ErrorData::new(
                ErrorCode::INTERNAL_ERROR,
                "Style-agent is disabled (no db_path configured)",
                None::<serde_json::Value>,
            )),
        }
    }
}

#[tool_handler]
impl ServerHandler for McpGateway {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Galoy Agents MCP Gateway")
    }
}
