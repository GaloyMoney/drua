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
use galoy_agents_memory::{
    ListMemoriesParams, MemoryEndpoints, SearchMemoryParams, StoreMemoryParams,
};
use style_agent_server::{SearchCodeParams, StyleAgentEndpoints};

pub use galoy_agents_memory::MemoryConfig;
pub use style_agent_server::StyleAgentConfig;

#[derive(Clone)]
pub struct McpGateway {
    #[allow(dead_code)]
    app: App,
    style_agent: Option<StyleAgentEndpoints>,
    memory: MemoryEndpoints,
    tool_router: ToolRouter<Self>,
}

impl McpGateway {
    fn new(app: App, style_agent: Option<StyleAgentEndpoints>, memory: MemoryEndpoints) -> Self {
        Self {
            app,
            style_agent,
            memory,
            tool_router: Self::tool_router(),
        }
    }

    /// Build the MCP service, using `app.style_agent_logs()` as the request logger.
    ///
    /// Accepts an optional shared [`Embedder`] so the caller can reuse it for
    /// other services (e.g. the memory crate).  When `None`, falls back to
    /// `init_endpoints_with_logger` which creates its own embedder only if
    /// style-agent is actually enabled.
    pub fn service(
        app: App,
        style_agent_config: &StyleAgentConfig,
        embedder: Option<style_agent_core::embedder::Embedder>,
        memory: MemoryEndpoints,
    ) -> anyhow::Result<(
        StreamableHttpService<Self, LocalSessionManager>,
        Option<StyleAgentEndpoints>,
    )> {
        let logger = app.style_agent_logs().clone();
        let style_agent = if let Some(embedder) = embedder {
            style_agent_server::init_endpoints_with_embedder(style_agent_config, embedder, logger)?
        } else {
            style_agent_server::init_endpoints_with_logger(style_agent_config, logger)?
        };
        let svc = Self::build_service(app, style_agent.clone(), memory);
        Ok((svc, style_agent))
    }

    fn build_service(
        app: App,
        style_agent: Option<StyleAgentEndpoints>,
        memory: MemoryEndpoints,
    ) -> StreamableHttpService<Self, LocalSessionManager> {
        let config = StreamableHttpServerConfig {
            stateful_mode: false,
            json_response: true,
            ..Default::default()
        };
        StreamableHttpService::new(
            move || {
                Ok(McpGateway::new(
                    app.clone(),
                    style_agent.clone(),
                    memory.clone(),
                ))
            },
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

    /// Store a research finding, decision, or piece of knowledge for future agents.
    #[tool(
        description = "Store a research finding, decision, or piece of knowledge for future agents. Always store important findings before completing a task."
    )]
    async fn store_memory(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(params): Parameters<StoreMemoryParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Self::require_auth(&parts)?;
        self.memory.store_memory(params).await
    }

    /// Search stored memories and research reports.
    #[tool(
        description = "Search stored memories and research reports. Always search before starting research — someone may have already investigated your topic."
    )]
    async fn search_memory(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(params): Parameters<SearchMemoryParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Self::require_auth(&parts)?;
        self.memory.search_memory(params).await
    }

    /// List stored memories and research reports.
    #[tool(
        description = "List stored memories and research reports, optionally filtered by project or type."
    )]
    async fn list_memories(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(params): Parameters<ListMemoriesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Self::require_auth(&parts)?;
        self.memory.list_memories(params).await
    }
}

#[tool_handler]
impl ServerHandler for McpGateway {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Galoy Agents MCP Gateway")
    }
}
