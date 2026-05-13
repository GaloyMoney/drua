use rmcp::model::{CallToolResult, JsonObject, Tool};

use crate::auth::AuthSubject;
use crate::toolset::{SearchableToolSet, ToolSetEntry, ToolSetScope, ToolSetsError, TunnelRoute};

use super::{RegisteredToolSet, TunnelHandle};

pub struct OwnedTunnelToolSet {
    name: String,
    prefix: String,
    category: String,
    category_description: String,
    upstream_name: String,
    tools: Vec<ToolSetEntry>,
    handle: TunnelHandle,
    scope: ToolSetScope,
}

impl OwnedTunnelToolSet {
    pub fn new(
        deployment_id: &str,
        session_id: uuid::Uuid,
        registration: &RegisteredToolSet,
        handle: TunnelHandle,
    ) -> Result<Self, String> {
        let tools: Vec<ToolSetEntry> = registration
            .tools
            .iter()
            .filter_map(|t| {
                let tool: Tool = serde_json::from_value(t.clone()).ok()?;
                Some(ToolSetEntry {
                    name: tool.name.to_string(),
                    description: tool,
                })
            })
            .collect();

        let name = format!("{}_{}", deployment_id, registration.name).replace('-', "_");
        let prefix = format!("{}_{}", deployment_id, registration.prefix).replace('-', "_");

        Ok(Self {
            name,
            prefix,
            category: registration.category.clone(),
            category_description: registration.category_description.clone(),
            upstream_name: registration.name.clone(),
            tools,
            handle,
            scope: ToolSetScope::Tunnel {
                deployment_id: deployment_id.to_string(),
                session_id,
                route: TunnelRoute::Owned,
            },
        })
    }
}

#[async_trait::async_trait]
impl SearchableToolSet for OwnedTunnelToolSet {
    fn name(&self) -> &str {
        &self.name
    }

    fn prefix(&self) -> &str {
        &self.prefix
    }

    fn category(&self) -> &str {
        &self.category
    }

    fn category_description(&self) -> &str {
        &self.category_description
    }

    fn tools(&self) -> &[ToolSetEntry] {
        &self.tools
    }

    fn scope(&self) -> Option<&ToolSetScope> {
        Some(&self.scope)
    }

    async fn call(
        &self,
        _subject: &AuthSubject,
        tool_name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        self.handle
            .call_tool(&self.upstream_name, tool_name, arguments)
            .await
            .map_err(|e| ToolSetsError::Tunnel(e.to_string()))
    }
}
