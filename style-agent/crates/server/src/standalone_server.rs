use std::sync::Arc;

use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router, ServerHandler,
};

use style_agent_core::search::SearchEngine;

use crate::config::StyleAgentConfig;
use crate::endpoints::{SearchCodeParams, StyleAgentEndpoints};
use crate::request_logger::NoopRequestLogger;

#[derive(Clone)]
pub struct StyleAgentServer {
    endpoints: StyleAgentEndpoints,
    tool_router: ToolRouter<Self>,
}

impl std::fmt::Debug for StyleAgentServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StyleAgentServer").finish_non_exhaustive()
    }
}

#[tool_router]
impl StyleAgentServer {
    pub fn new(search_engine: Arc<SearchEngine>) -> Self {
        let logger = Arc::new(NoopRequestLogger);
        Self {
            endpoints: StyleAgentEndpoints::with_logger(search_engine, logger),
            tool_router: Self::tool_router(),
        }
    }

    /// Search indexed code repositories for patterns, conventions, and style examples matching a query.
    #[tool(
        description = "Search indexed codebases for code patterns matching a query.\n\nUsage tips:\n- Pass a code snippet as the query (e.g. the pattern you are about to write) — code-as-query gives much better results than natural language\n- Always pass a `label` filter for precise results\n- Adopt the style, naming, and structure from the returned examples — don't guess conventions, search first\n\nAvailable labels: entity, entity_event, entity_command, entity_query, entity_hydration, error, service, service_method, repository, domain_primitives, value_object, type_conversion, config, test, api, job, event_handler, authorization, published_event, new_entity, none (unlabeled chunks)\n\nAvailable filters: query (required), limit, repo, language, label"
    )]
    async fn search_code(
        &self,
        Parameters(params): Parameters<SearchCodeParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        self.endpoints.search_code(params).await
    }
}

#[tool_handler]
impl ServerHandler for StyleAgentServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "style-agent",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Style Agent — searches indexed code repositories for patterns and conventions",
            )
    }
}

/// Handler for `GET /health` — simple liveness check.
async fn health_handler() -> &'static str {
    "ok"
}

/// Build the style-agent axum [`Router`] with `/mcp`, `/stats`, and `/health` routes.
///
/// Use this to embed style-agent routes in an existing axum server via
/// [`axum::Router::nest`]. For a standalone server, use [`run_server`].
pub fn router(search_engine: Arc<SearchEngine>) -> axum::Router {
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    };

    let config = StreamableHttpServerConfig {
        stateful_mode: false,
        json_response: true,
        ..Default::default()
    };

    let service = StreamableHttpService::new(
        move || Ok(StyleAgentServer::new(Arc::clone(&search_engine))),
        LocalSessionManager::default().into(),
        config,
    );

    axum::Router::new()
        .route("/health", axum::routing::get(health_handler))
        .nest_service("/mcp", service)
}

/// Initialise the style-agent search engine and return its axum router.
pub fn init_router(config: &StyleAgentConfig) -> anyhow::Result<axum::Router> {
    let endpoints = crate::endpoints::init_endpoints(config)?.ok_or_else(|| {
        anyhow::anyhow!("db_path must be set to run the standalone style-agent server")
    })?;
    Ok(router(Arc::clone(&endpoints.search_engine)))
}

/// Start the HTTP MCP server from a `CoreConfig` and block until shutdown.
pub async fn run_server_with_config(
    config: &style_agent_core::CoreConfig,
    bind_addr: &str,
) -> anyhow::Result<()> {
    use style_agent_core::embedder::Embedder;
    use style_agent_core::store::VectorStore;

    let embedder = Embedder::new()?;
    let store = VectorStore::new(&config.db_path)?;
    store.ensure_collection()?;
    store.ensure_anti_pattern_tables()?;
    let search_engine = Arc::new(SearchEngine::new(embedder, store));

    run_server(search_engine, bind_addr).await
}

/// Start the HTTP MCP server and block until shutdown.
pub async fn run_server(search_engine: Arc<SearchEngine>, bind_addr: &str) -> anyhow::Result<()> {
    let app = router(search_engine);
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    let url = format!("http://{bind_addr}/mcp");

    tracing::info!(%url, "Style Agent MCP server listening");
    println!("Style Agent MCP server listening at {url}");

    axum::serve(listener, app).await?;
    Ok(())
}
