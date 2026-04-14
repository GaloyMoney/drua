//! Dynamic MCP server.
//!
//! Tools are sourced live from the core `ToolSets` registry via
//! [`top_level_tools`](galoy_agents_core::toolset::ToolSets::top_level_tools)
//! / [`call_top_level_tool`](galoy_agents_core::toolset::ToolSets::call_top_level_tool).
//! There is no static `#[tool]`-annotated table — `list_tools` reflects
//! whatever the registry currently exposes for the calling subject's scopes,
//! and `call_tool` dispatches by name straight into the registry.

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResult, ErrorCode, ErrorData, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use rmcp::{RoleServer, ServerHandler};

use galoy_agents_core::audit::primitives::InteractionType;
use galoy_agents_core::audit::Audit;
use galoy_agents_core::auth::AuthSubject;
use galoy_agents_core::toolset::{estimate_tokens, TopLevelTool};
use galoy_agents_core::App;

#[derive(Clone)]
pub struct McpGateway {
    app: App,
}

impl McpGateway {
    fn new(app: App) -> Self {
        Self { app }
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

    fn assert_can_see_server(ctx: &RequestContext<RoleServer>) -> Result<&AuthSubject, ErrorData> {
        // The streamable-http transport stuffs the original axum request's
        // `http::request::Parts` into `ctx.extensions`. The `AuthSubject`
        // that `auth_middleware` injected lives on `parts.extensions`, not
        // on `ctx.extensions` directly.
        let auth = ctx
            .extensions
            .get::<http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<AuthSubject>());
        match auth {
            Some(auth @ AuthSubject::ExportedAgent(_, _, _))
            | Some(auth @ AuthSubject::Agent(_, _, _)) => Ok(auth),
            _ => Err(ErrorData::new(
                ErrorCode::INVALID_REQUEST,
                "Authentication required: provide a valid Bearer token",
                None::<serde_json::Value>,
            )),
        }
    }
}

/// Convert a registered top-level tool into the rmcp wire shape.
fn to_mcp_tool(tool: &Arc<dyn TopLevelTool>) -> Tool {
    let input_schema = match tool.input_schema() {
        serde_json::Value::Object(map) => map.clone(),
        // Defensive: TopLevelTool::input_schema is contractually an object;
        // fall back to an empty object rather than panic if a tool ever
        // returns something else.
        _ => serde_json::Map::new(),
    };
    let mut t = Tool::default();
    t.name = tool.name().to_string().into();
    t.description = Some(tool.description().to_string().into());
    t.input_schema = Arc::new(input_schema);
    t
}

impl ServerHandler for McpGateway {
    fn get_info(&self) -> ServerInfo {
        let instructions = format!(
            "Galoy Agents MCP Gateway\n\n{}",
            self.app.toolsets().mcp_gateway_info()
        );
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(instructions)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let auth = Self::assert_can_see_server(&ctx)?;
        let tools: Vec<Tool> = self
            .app
            .toolsets()
            .top_level_tools(auth)
            .map(to_mcp_tool)
            .collect();
        Ok(ListToolsResult {
            tools,
            next_cursor: None,
            meta: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        use es_entity::context::{EventContext, WithEventContext};

        let auth = Self::assert_can_see_server(&ctx)?;
        let name = request.name.as_ref();

        // The rmcp transport spawns a task so the web middleware's
        // EventContext does not propagate here. Seed a fresh one for
        // MCP-specific audit fields.
        let seed = {
            let ctx = EventContext::current();
            ctx.data()
        };

        let app = self.app.clone();
        let auth_clone = auth.clone();
        let args = request.arguments;

        async move {
            Audit::record_interaction_type(InteractionType::McpCall);
            Audit::record_subject(&auth_clone);
            Audit::record_action(name);
            let args_value = args
                .as_ref()
                .map(|a| serde_json::Value::Object(a.clone()));
            Audit::record_metadata(serde_json::json!({
                "tool_name": name,
                "arguments": args_value,
            }));

            let start = std::time::Instant::now();
            let result = app
                .toolsets()
                .call_top_level_tool(&auth_clone, name, args)
                .await;
            Audit::record_duration(start);

            match &result {
                Ok(r) => {
                    Audit::record_tokens(estimate_tokens(r));
                    Audit::record_success();
                }
                Err(e) => {
                    Audit::record_error(e.to_string());
                }
            }

            app.audit().record_from_context();

            result.map_err(|e| {
                ErrorData::new(
                    ErrorCode::INTERNAL_ERROR,
                    e.to_string(),
                    None::<serde_json::Value>,
                )
            })
        }
        .with_event_context(seed)
        .await
    }
}
