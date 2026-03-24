use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{schemars, tool, tool_handler, tool_router, ServerHandler};

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
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct HelloParams {
    name: String,
}

#[tool_router]
impl McpGateway {
    #[tool(description = "Say hello")]
    async fn hello(&self, Parameters(params): Parameters<HelloParams>) -> String {
        format!("Hello, {}!", params.name)
    }
}

#[tool_handler]
impl ServerHandler for McpGateway {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Galoy Agents MCP Gateway")
    }
}
