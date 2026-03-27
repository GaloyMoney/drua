mod concourse;

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

pub use concourse::ConcourseConfig;
use concourse::ConcourseEndpoints;
pub use style_agent_server::StyleAgentConfig;

#[derive(Clone)]
pub struct McpGateway {
    #[allow(dead_code)]
    app: App,
    style_agent: Option<StyleAgentEndpoints>,
    concourse: Option<ConcourseEndpoints>,
    tool_router: ToolRouter<Self>,
}

impl McpGateway {
    fn new(
        app: App,
        style_agent: Option<StyleAgentEndpoints>,
        concourse: Option<ConcourseEndpoints>,
    ) -> Self {
        Self {
            app,
            style_agent,
            concourse,
            tool_router: Self::tool_router(),
        }
    }

    /// Build the MCP service, using `app.style_agent_logs()` as the request logger.
    pub fn service(
        app: App,
        style_agent_config: &StyleAgentConfig,
        concourse_config: &ConcourseConfig,
    ) -> anyhow::Result<(
        StreamableHttpService<Self, LocalSessionManager>,
        Option<StyleAgentEndpoints>,
    )> {
        let logger = app.style_agent_logs().clone();
        let style_agent =
            style_agent_server::init_endpoints_with_logger(style_agent_config, logger)?;
        let concourse = ConcourseEndpoints::try_new(concourse_config)?;
        let svc = Self::build_service(app, style_agent.clone(), concourse);
        Ok((svc, style_agent))
    }

    fn build_service(
        app: App,
        style_agent: Option<StyleAgentEndpoints>,
        concourse: Option<ConcourseEndpoints>,
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
                    concourse.clone(),
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

    fn require_concourse(&self) -> Result<&ConcourseEndpoints, ErrorData> {
        self.concourse.as_ref().ok_or_else(|| {
            ErrorData::new(
                ErrorCode::INTERNAL_ERROR,
                "Concourse integration is disabled",
                None::<serde_json::Value>,
            )
        })
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct HelloParams {
    name: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ListPipelinesParams {
    /// Optional team name to scope results. When omitted, lists pipelines across all teams.
    team: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ListJobsParams {
    /// The pipeline name to list jobs for
    pipeline: String,
    /// Optional team name. Falls back to the configured default team if omitted.
    team: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct GetBuildStatusParams {
    /// The pipeline name
    pipeline: String,
    /// The job name
    job: String,
    /// Optional team name. Falls back to the configured default team if omitted.
    team: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct GetBuildLogsParams {
    /// The numeric build ID
    build_id: i64,
    /// Starting line offset for paginated reads (enables live tailing mode).
    /// When omitted, returns all logs at once (best for finished builds).
    /// Use 0 for the first poll, then use `next_offset` from the response.
    offset: Option<usize>,
    /// Maximum number of lines to return per request (default: 200).
    /// Only used when `offset` is provided.
    limit: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct TriggerJobParams {
    /// The pipeline name
    pipeline: String,
    /// The job name to trigger
    job: String,
    /// Optional team name. Falls back to the configured default team if omitted.
    team: Option<String>,
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

    /// List all Concourse teams.
    #[tool(
        description = "List all Concourse CI teams the authenticated user has access to. Returns team IDs and names."
    )]
    async fn list_teams(
        &self,
        Extension(parts): Extension<http::request::Parts>,
    ) -> Result<CallToolResult, ErrorData> {
        Self::require_auth(&parts)?;
        self.require_concourse()?.list_teams().await
    }

    /// List Concourse pipelines.
    #[tool(
        description = "List Concourse CI pipelines. By default lists pipelines across all teams. Pass `team` to scope to a specific team. Returns pipeline names, paused/archived status, and team."
    )]
    async fn list_pipelines(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(params): Parameters<ListPipelinesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Self::require_auth(&parts)?;
        self.require_concourse()?
            .list_pipelines(params.team.as_deref())
            .await
    }

    /// List jobs in a Concourse pipeline.
    #[tool(
        description = "List jobs in a Concourse pipeline. Returns job names, paused state, and last build status. Uses the configured default team unless `team` is specified."
    )]
    async fn list_jobs(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(params): Parameters<ListJobsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Self::require_auth(&parts)?;
        self.require_concourse()?
            .list_jobs(&params.pipeline, params.team.as_deref())
            .await
    }

    /// Get the latest build status for a Concourse job.
    #[tool(
        description = "Get the latest build status for a specific job in a Concourse pipeline. Returns build ID, status, and timestamps. Uses the configured default team unless `team` is specified."
    )]
    async fn get_build_status(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(params): Parameters<GetBuildStatusParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Self::require_auth(&parts)?;
        self.require_concourse()?
            .get_build_status(&params.pipeline, &params.job, params.team.as_deref())
            .await
    }

    /// Get build output/logs from Concourse.
    #[tool(
        description = "Get build output/logs for a Concourse build by its numeric build ID.\n\nTwo modes:\n- **All-at-once** (offset omitted): returns complete log output as plain text. Best for finished builds.\n- **Live tailing** (offset provided): returns paginated lines with next_offset for polling. Use offset=0 for the first call, then pass next_offset from the response. Response includes is_complete and build_status fields.\n\nExample polling loop: call with offset=0, then keep calling with next_offset until is_complete is true."
    )]
    async fn get_build_logs(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(params): Parameters<GetBuildLogsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Self::require_auth(&parts)?;
        self.require_concourse()?
            .get_build_logs(params.build_id, params.offset, params.limit)
            .await
    }

    /// Trigger a new build for a Concourse job.
    #[tool(
        description = "Trigger a new build for a job in a Concourse pipeline. Takes pipeline and job name, returns the new build ID, status, and URL. Uses the configured default team unless `team` is specified."
    )]
    async fn trigger_job(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(params): Parameters<TriggerJobParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Self::require_auth(&parts)?;
        self.require_concourse()?
            .trigger_job(&params.pipeline, &params.job, params.team.as_deref())
            .await
    }
}

#[tool_handler]
impl ServerHandler for McpGateway {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Galoy Agents MCP Gateway")
    }
}
