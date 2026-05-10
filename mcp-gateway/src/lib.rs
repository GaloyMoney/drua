//! Dynamic MCP server.
//!
//! Tools are sourced live from the core `ToolSets` registry via
//! [`top_level_tools`](drua_core::toolset::ToolSets::top_level_tools)
//! / [`call_top_level_tool`](drua_core::toolset::ToolSets::call_top_level_tool).
//! There is no static `#[tool]`-annotated table — `list_tools` reflects
//! whatever the registry currently exposes for the calling subject's scopes,
//! and `call_tool` dispatches by name straight into the registry.

use std::sync::Arc;

use serde_json::{Map, Value};

use rmcp::model::{
    CallToolRequestParams, CallToolResult, ErrorCode, ErrorData, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use rmcp::{RoleServer, ServerHandler};

use drua_core::auth::AuthSubject;
use drua_core::toolset::TopLevelTool;
use drua_core::App;

#[derive(Clone)]
pub struct McpGateway {
    app: App,
}

impl McpGateway {
    fn new(app: App) -> Self {
        Self { app }
    }

    /// Build the MCP service.
    ///
    /// Host-header validation is disabled because the server runs behind
    /// an ingress controller that already enforces the correct hostname.
    /// Authentication (Bearer token) is required for all MCP operations.
    pub fn service(app: App) -> StreamableHttpService<Self, LocalSessionManager> {
        let mut config = StreamableHttpServerConfig::default().disable_allowed_hosts();
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
fn to_mcp_tool(tool: &(dyn TopLevelTool + '_)) -> Tool {
    let input_schema = match tool.input_schema() {
        Value::Object(map) => sanitized_mcp_schema(map),
        // Defensive: TopLevelTool::input_schema is contractually an object;
        // fall back to an empty object rather than panic if a tool ever
        // returns something else.
        _ => Map::new(),
    };
    let output_schema = tool.output_schema().and_then(|v| match v {
        Value::Object(map) => Some(Arc::new(sanitized_mcp_schema(map))),
        _ => None,
    });
    let mut t = Tool::default();
    t.name = tool.name().to_string().into();
    t.description = Some(tool.description().to_string().into());
    t.input_schema = Arc::new(input_schema);
    t.output_schema = output_schema;
    t
}

fn sanitized_mcp_schema(schema: &Map<String, Value>) -> Map<String, Value> {
    let mut value = Value::Object(schema.clone());
    remove_nonstandard_unsigned_integer_formats(&mut value);
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

fn remove_nonstandard_unsigned_integer_formats(value: &mut Value) {
    match value {
        Value::Object(map) => {
            let is_unsigned_integer = map.get("type").and_then(Value::as_str) == Some("integer")
                && map
                    .get("format")
                    .and_then(Value::as_str)
                    .is_some_and(is_unsigned_integer_format);

            if is_unsigned_integer {
                map.remove("format");
                map.entry("minimum".to_string()).or_insert(Value::from(0));
            }

            for child in map.values_mut() {
                remove_nonstandard_unsigned_integer_formats(child);
            }
        }
        Value::Array(items) => {
            for child in items {
                remove_nonstandard_unsigned_integer_formats(child);
            }
        }
        _ => {}
    }
}

fn is_unsigned_integer_format(format: &str) -> bool {
    matches!(
        format,
        "uint" | "uint8" | "uint16" | "uint32" | "uint64" | "usize"
    )
}

impl ServerHandler for McpGateway {
    fn get_info(&self) -> ServerInfo {
        let instructions = format!(
            "Drua MCP Gateway\n\n{}",
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
            .top_level_tool_arcs(auth)
            .map(|t| to_mcp_tool(t.as_ref()))
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
        let auth = Self::assert_can_see_server(&ctx)?;
        let name = request.name.as_ref();

        self.app
            .toolsets()
            .call_top_level_tool(auth, name, request.arguments)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_unsigned_integer_formats_recursively() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "total": { "type": "integer", "format": "uint" },
                "nested": {
                    "type": "object",
                    "properties": {
                        "execution_time_ms": { "type": "integer", "format": "uint64" },
                        "seq": { "type": "integer", "format": "uint32", "minimum": 1 }
                    }
                },
                "items": {
                    "type": "array",
                    "items": { "type": "integer", "format": "uint16" }
                }
            }
        });
        let sanitized = sanitized_mcp_schema(schema.as_object().expect("object schema"));

        assert_eq!(sanitized["properties"]["total"]["type"], "integer");
        assert_eq!(sanitized["properties"]["total"]["minimum"], 0);
        assert!(sanitized["properties"]["total"].get("format").is_none());

        assert!(
            sanitized["properties"]["nested"]["properties"]["execution_time_ms"]
                .get("format")
                .is_none()
        );
        assert_eq!(
            sanitized["properties"]["nested"]["properties"]["execution_time_ms"]["minimum"],
            0
        );

        assert!(sanitized["properties"]["nested"]["properties"]["seq"]
            .get("format")
            .is_none());
        assert_eq!(
            sanitized["properties"]["nested"]["properties"]["seq"]["minimum"],
            1
        );

        assert!(sanitized["properties"]["items"]["items"]
            .get("format")
            .is_none());
    }

    #[test]
    fn leaves_non_unsigned_formats_unchanged() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "created_at": { "type": "string", "format": "date-time" },
                "signed": { "type": "integer", "format": "int64" }
            }
        });
        let sanitized = sanitized_mcp_schema(schema.as_object().expect("object schema"));

        assert_eq!(sanitized["properties"]["created_at"]["format"], "date-time");
        assert_eq!(sanitized["properties"]["signed"]["format"], "int64");
        assert!(sanitized["properties"]["signed"].get("minimum").is_none());
    }
}
