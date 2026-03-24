use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ErrorCode, ErrorData, ServerCapabilities, ServerInfo};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use rmcp::{schemars, tool, tool_handler, tool_router, ServerHandler};

use galoy_agents_domain::auth::AuthContext;
use galoy_agents_domain::App;

#[derive(Clone)]
pub struct McpGateway {
    #[allow(dead_code)]
    app: App,
    tool_router: ToolRouter<Self>,
}

impl McpGateway {
    pub fn new(app: App) -> Self {
        Self {
            app,
            tool_router: Self::tool_router(),
        }
    }

    pub fn service(app: App) -> StreamableHttpService<Self, LocalSessionManager> {
        let config = StreamableHttpServerConfig {
            stateful_mode: false,
            json_response: true,
            ..Default::default()
        };
        StreamableHttpService::new(
            move || Ok(McpGateway::new(app.clone())),
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
}

#[tool_handler]
impl ServerHandler for McpGateway {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Galoy Agents MCP Gateway")
    }
}
