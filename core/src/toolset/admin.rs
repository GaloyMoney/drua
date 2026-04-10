use std::sync::Arc;

use rmcp::model::{CallToolResult, Content, JsonObject, Tool};
use tracing::instrument;

use crate::agent::Agents;
use crate::auth::AuthContext;
use crate::primitives::*;
use crate::workspace::Workspaces;

use super::{ToolSet, ToolSetEntry, ToolSetsError};

pub struct AdminToolSet {
    workspaces: Workspaces,
    agents: Arc<Agents>,
    tools: Vec<ToolSetEntry>,
}

impl AdminToolSet {
    pub fn new(workspaces: Workspaces, agents: Arc<Agents>) -> Self {
        let tools = vec![
            tool_entry(
                "list_workspaces",
                "List all workspaces. Returns workspace IDs, names, descriptions, and creation dates.",
                serde_json::json!({"type": "object", "properties": {}}),
            ),
            tool_entry(
                "create_workspace",
                "Create a new workspace (and its lead agent). Returns the created workspace.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Workspace name"},
                        "description": {"type": "string", "description": "Optional workspace description"}
                    },
                    "required": ["name"]
                }),
            ),
            tool_entry(
                "get_workspace",
                "Get workspace details by ID.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "workspace_id": {"type": "string", "description": "The workspace UUID"}
                    },
                    "required": ["workspace_id"]
                }),
            ),
            tool_entry(
                "update_workspace",
                "Update a workspace's name and/or description.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "workspace_id": {"type": "string", "description": "The workspace UUID"},
                        "name": {"type": "string", "description": "New workspace name"},
                        "description": {"type": "string", "description": "New workspace description (omit to clear)"}
                    },
                    "required": ["workspace_id", "name"]
                }),
            ),
            tool_entry(
                "delete_workspace",
                "Archive (soft-delete) a workspace and revoke MCP credentials for all its agents.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "workspace_id": {"type": "string", "description": "The workspace UUID to delete"}
                    },
                    "required": ["workspace_id"]
                }),
            ),
            tool_entry(
                "list_agents",
                "List agents in a workspace. Returns agent IDs, names, types, and status.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "workspace_id": {"type": "string", "description": "The workspace UUID"}
                    },
                    "required": ["workspace_id"]
                }),
            ),
            tool_entry(
                "get_agent",
                "Get agent details by ID.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {"type": "string", "description": "The agent UUID"}
                    },
                    "required": ["agent_id"]
                }),
            ),
            tool_entry(
                "send_agent_message",
                "Send a message to an agent and wait for the full response. Blocks until the agent completes (up to 120 seconds). Returns the agent's text response.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {"type": "string", "description": "The agent UUID"},
                        "message": {"type": "string", "description": "The prompt to send to the agent"}
                    },
                    "required": ["agent_id", "message"]
                }),
            ),
        ];

        Self {
            workspaces,
            agents,
            tools,
        }
    }

    fn extract_user_id(auth: Option<&AuthContext>) -> Result<UserId, ToolSetsError> {
        match auth {
            Some(AuthContext::ExportedAgent(user_id, _, _)) => Ok(*user_id),
            Some(AuthContext::Agent(_, _, _)) => Err(ToolSetsError::Unauthorized),
            Some(AuthContext::User(user_id)) => Ok(*user_id),
            _ => Err(ToolSetsError::Unauthorized),
        }
    }

    #[instrument(name = "toolset.admin.list_workspaces", skip_all)]
    async fn handle_list_workspaces(&self) -> Result<CallToolResult, ToolSetsError> {
        let workspaces = self
            .workspaces
            .list_all()
            .await
            .map_err(|e| ToolSetsError::Admin(e.to_string()))?;
        let summary: Vec<serde_json::Value> = workspaces
            .iter()
            .map(|ws| {
                serde_json::json!({
                    "id": ws.id.to_string(),
                    "name": ws.name,
                    "description": ws.description,
                    "archived": ws.is_archived(),
                    "created_at": ws.created_at().to_rfc3339(),
                })
            })
            .collect();
        Ok(text_result(&summary))
    }

    #[instrument(name = "toolset.admin.create_workspace", skip_all)]
    async fn handle_create_workspace(
        &self,
        args: &JsonObject,
        auth: Option<&AuthContext>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let user_id = Self::extract_user_id(auth)?;
        let name = str_arg(args, "name")?;
        let description = args
            .get("description")
            .and_then(|v| v.as_str())
            .map(String::from);
        let ws = self
            .workspaces
            .create(user_id, name, description)
            .await
            .map_err(|e| ToolSetsError::Admin(e.to_string()))?;
        let result = serde_json::json!({
            "id": ws.id.to_string(),
            "name": ws.name,
            "description": ws.description,
            "created_at": ws.created_at().to_rfc3339(),
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    #[instrument(name = "toolset.admin.get_workspace", skip_all)]
    async fn handle_get_workspace(
        &self,
        args: &JsonObject,
    ) -> Result<CallToolResult, ToolSetsError> {
        let workspace_id = parse_uuid_arg(args, "workspace_id")?;
        let ws = self
            .workspaces
            .find_by_id(WorkspaceId::from(workspace_id))
            .await
            .map_err(|e| ToolSetsError::Admin(e.to_string()))?;
        let result = serde_json::json!({
            "id": ws.id.to_string(),
            "name": ws.name,
            "description": ws.description,
            "archived": ws.is_archived(),
            "created_at": ws.created_at().to_rfc3339(),
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    #[instrument(name = "toolset.admin.update_workspace", skip_all)]
    async fn handle_update_workspace(
        &self,
        args: &JsonObject,
    ) -> Result<CallToolResult, ToolSetsError> {
        let workspace_id = parse_uuid_arg(args, "workspace_id")?;
        let name = str_arg(args, "name")?;
        let description = args
            .get("description")
            .and_then(|v| v.as_str())
            .map(String::from);
        let ws = self
            .workspaces
            .update(WorkspaceId::from(workspace_id), name, description)
            .await
            .map_err(|e| ToolSetsError::Admin(e.to_string()))?;
        let result = serde_json::json!({
            "id": ws.id.to_string(),
            "name": ws.name,
            "description": ws.description,
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    #[instrument(name = "toolset.admin.delete_workspace", skip_all)]
    async fn handle_delete_workspace(
        &self,
        args: &JsonObject,
    ) -> Result<CallToolResult, ToolSetsError> {
        let workspace_id = parse_uuid_arg(args, "workspace_id")?;
        let ws = self
            .workspaces
            .delete(WorkspaceId::from(workspace_id))
            .await
            .map_err(|e| ToolSetsError::Admin(e.to_string()))?;
        let result = serde_json::json!({
            "id": ws.id.to_string(),
            "name": ws.name,
            "archived": ws.is_archived(),
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    #[instrument(name = "toolset.admin.list_agents", skip_all)]
    async fn handle_list_agents(&self, args: &JsonObject) -> Result<CallToolResult, ToolSetsError> {
        let workspace_id = parse_uuid_arg(args, "workspace_id")?;
        let agents = self
            .agents
            .list_for_workspace(WorkspaceId::from(workspace_id))
            .await
            .map_err(|e| ToolSetsError::Admin(e.to_string()))?;
        let summary: Vec<serde_json::Value> = agents
            .iter()
            .map(|a| {
                serde_json::json!({
                    "id": a.id.to_string(),
                    "name": a.name,
                    "agent_type": a.agent_type,
                    "workspace_id": a.workspace_id.to_string(),
                })
            })
            .collect();
        Ok(text_result(&summary))
    }

    #[instrument(name = "toolset.admin.get_agent", skip_all)]
    async fn handle_get_agent(&self, args: &JsonObject) -> Result<CallToolResult, ToolSetsError> {
        let agent_id = parse_uuid_arg(args, "agent_id")?;
        let agent = self
            .agents
            .find_by_id(AgentId::from(agent_id))
            .await
            .map_err(|e| ToolSetsError::Admin(e.to_string()))?;
        let result = serde_json::json!({
            "id": agent.id.to_string(),
            "name": agent.name,
            "agent_type": agent.agent_type,
            "workspace_id": agent.workspace_id.to_string(),
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    #[instrument(name = "toolset.admin.send_agent_message", skip(self, args, auth))]
    async fn handle_send_agent_message(
        &self,
        args: &JsonObject,
        auth: Option<&AuthContext>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let user_id = Self::extract_user_id(auth)?;
        let agent_id = parse_uuid_arg(args, "agent_id")?;
        let message = str_arg(args, "message")?;

        let mut rx = self
            .agents
            .send_message(AgentId::from(agent_id), user_id, message.to_string())
            .await
            .map_err(|e| ToolSetsError::Admin(e.to_string()))?;

        // Collect all text events until Done or Error, with a timeout
        let mut texts = Vec::new();
        let timeout = tokio::time::Duration::from_secs(120);
        let result = tokio::time::timeout(timeout, async {
            while let Some(event) = rx.recv().await {
                match event {
                    AgentMessageEvent::Text { text } => texts.push(text),
                    AgentMessageEvent::Done { .. } => break,
                    AgentMessageEvent::Error { message } => {
                        return Err(ToolSetsError::Admin(format!("Agent error: {message}")));
                    }
                    _ => {} // skip tool calls, service events
                }
            }
            Ok(())
        })
        .await;

        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(ToolSetsError::Admin(
                    "Agent response timed out after 120 seconds".to_string(),
                ));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(texts.join(""))]))
    }
}

#[async_trait::async_trait]
impl ToolSet for AdminToolSet {
    fn name(&self) -> &str {
        "admin"
    }

    fn category(&self) -> &str {
        "admin"
    }

    fn category_description(&self) -> &str {
        "Privileged operations: workspace management, agent control, environment verification"
    }

    fn tools(&self) -> &[ToolSetEntry] {
        &self.tools
    }

    fn required_scopes(&self) -> &[&str] {
        &["admin"]
    }

    async fn call(
        &self,
        tool_name: &str,
        arguments: Option<JsonObject>,
        auth: Option<&AuthContext>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let args = arguments.unwrap_or_default();
        match tool_name {
            "list_workspaces" => self.handle_list_workspaces().await,
            "create_workspace" => self.handle_create_workspace(&args, auth).await,
            "get_workspace" => self.handle_get_workspace(&args).await,
            "update_workspace" => self.handle_update_workspace(&args).await,
            "delete_workspace" => self.handle_delete_workspace(&args).await,
            "list_agents" => self.handle_list_agents(&args).await,
            "get_agent" => self.handle_get_agent(&args).await,
            "send_agent_message" => self.handle_send_agent_message(&args, auth).await,
            _ => Err(ToolSetsError::ToolNotFound(tool_name.to_string())),
        }
    }
}

fn tool_entry(name: &str, description: &str, schema: serde_json::Value) -> ToolSetEntry {
    let input_schema: JsonObject = match schema {
        serde_json::Value::Object(m) => m,
        _ => Default::default(),
    };
    let mut tool = Tool::default();
    tool.name = name.to_string().into();
    tool.description = Some(description.to_string().into());
    tool.input_schema = Arc::new(input_schema);
    ToolSetEntry {
        name: name.to_string(),
        description: tool,
        default_output_filter: None,
    }
}

fn text_result(value: &[serde_json::Value]) -> CallToolResult {
    CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(value).unwrap_or_default(),
    )])
}

fn str_arg<'a>(args: &'a JsonObject, key: &str) -> Result<&'a str, ToolSetsError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolSetsError::MissingArgument(key.to_string()))
}

fn parse_uuid_arg(args: &JsonObject, key: &str) -> Result<uuid::Uuid, ToolSetsError> {
    let s = str_arg(args, key)?;
    s.parse::<uuid::Uuid>()
        .map_err(|e| ToolSetsError::InvalidArgument(format!("invalid UUID for '{key}': {e}")))
}
