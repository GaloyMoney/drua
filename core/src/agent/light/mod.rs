mod types;

use tokio::sync::mpsc;
use tracing::instrument;

use crate::toolset::Catalog;

use self::types::*;
use super::config::LightRuntimeConfig;
use super::entity::ChatConfig;
use super::error::AgentError;
use super::AgentMessageEvent;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";

// ---------------------------------------------------------------------------
// Anthropic HTTP client
// ---------------------------------------------------------------------------

struct AnthropicClient {
    http: reqwest::Client,
    api_key: String,
}

impl AnthropicClient {
    fn new(api_key: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key,
        }
    }

    async fn create_message(
        &self,
        request: &MessagesRequest<'_>,
    ) -> Result<MessagesResponse, AgentError> {
        let resp = self
            .http
            .post(API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json")
            .json(request)
            .send()
            .await
            .map_err(AgentError::Http)?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(AgentError::AnthropicApi {
                status: status.as_u16(),
                message: text,
            });
        }

        resp.json().await.map_err(AgentError::Http)
    }
}

// ---------------------------------------------------------------------------
// Tool definitions (progressive-disclosure meta-tools)
// ---------------------------------------------------------------------------

/// The 3 meta-tool definitions for the Anthropic Messages API.
///
/// These map to `Catalog::search`, `Catalog::describe`, and
/// `Catalog::call_with_filter` respectively.
fn meta_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "search_tools".to_string(),
            description: "Search for available tools across all upstream services. Returns tool names, brief descriptions, and categories. Use this first to find relevant tools before calling them.\n\nTip: Use describe_tool to get full parameter schemas before calling a tool.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query (e.g., 'pipeline status', 'customer accounts', 'code review')"
                    },
                    "category": {
                        "type": "string",
                        "description": "Filter by service category (e.g., 'ci', 'observability', 'code-quality', or 'all')"
                    }
                }
            }),
        },
        ToolDefinition {
            name: "describe_tool".to_string(),
            description: "Get the full parameter schema and detailed description for a specific tool. Use after search_tools to understand how to call a tool.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tool_name": {
                        "type": "string",
                        "description": "The tool name returned from search_tools (e.g., 'honeycomb_list_environments')"
                    }
                },
                "required": ["tool_name"]
            }),
        },
        ToolDefinition {
            name: "call_tool".to_string(),
            description: "Execute an upstream tool by name with the provided arguments. Use describe_tool first to understand the required parameters.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tool_name": {
                        "type": "string",
                        "description": "The prefixed tool name (e.g., 'honeycomb_list_environments')"
                    },
                    "arguments": {
                        "type": "object",
                        "description": "Tool arguments matching the schema from describe_tool"
                    },
                    "output_filter": {
                        "type": "object",
                        "description": "Optional post-processing filter applied to tool output. Reduces output size to save tokens. By default, output is capped at 1000 lines.",
                        "properties": {
                            "grep": {
                                "type": "string",
                                "description": "Regex pattern to filter output lines (only matching lines returned)"
                            },
                            "invert_match": {
                                "type": "boolean",
                                "description": "Exclude matching lines instead of including them (grep -v). Default: false"
                            },
                            "context_lines": {
                                "type": "integer",
                                "description": "Lines of context around grep matches (grep -C). Only used with grep"
                            },
                            "head": {
                                "type": "integer",
                                "description": "Return only the first N lines"
                            },
                            "tail": {
                                "type": "integer",
                                "description": "Return only the last N lines"
                            }
                        }
                    }
                },
                "required": ["tool_name"]
            }),
        },
    ]
}

// ---------------------------------------------------------------------------
// Light agent session (groups agentic loop state)
// ---------------------------------------------------------------------------

struct LightSession {
    client: AnthropicClient,
    catalog: Catalog,
    model: String,
    max_tokens: u32,
    max_turns: usize,
    tools: Vec<ToolDefinition>,
    system: String,
}

impl LightSession {
    /// Route a tool call to the appropriate handler.
    async fn dispatch_tool(
        &self,
        name: &str,
        input: &serde_json::Value,
    ) -> Result<String, AgentError> {
        match name {
            "search_tools" => {
                let query = input.get("query").and_then(|v| v.as_str());
                let category = input.get("category").and_then(|v| v.as_str());
                let results = self.catalog.search(query, category).await;
                Ok(Catalog::format_search_results(&results))
            }
            "describe_tool" => {
                let tool_name = input
                    .get("tool_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let entry = self.catalog.describe(tool_name).await.ok_or_else(|| {
                    AgentError::SandboxExec(format!(
                        "Tool '{tool_name}' not found. Use search_tools to find available tools."
                    ))
                })?;
                Ok(Catalog::format_describe(&entry))
            }
            "call_tool" => {
                let tool_name = input
                    .get("tool_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let args: Option<rmcp::model::JsonObject> = input
                    .get("arguments")
                    .and_then(|v| serde_json::from_value(v.clone()).ok());
                let output_filter = input
                    .get("output_filter")
                    .and_then(|v| serde_json::from_value(v.clone()).ok());
                let result = self
                    .catalog
                    .call_with_filter(tool_name, args, output_filter)
                    .await
                    .map_err(|e| AgentError::SandboxExec(e.to_string()))?;
                Ok(call_result_to_text(&result))
            }
            _ => Err(AgentError::SandboxExec(format!(
                "Unknown tool: {name}. Use search_tools, describe_tool, or call_tool."
            ))),
        }
    }

    async fn run(
        &self,
        prompt: &str,
        tx: &mpsc::Sender<AgentMessageEvent>,
    ) -> Result<(), AgentError> {
        let mut messages: Vec<Message> = vec![Message::User {
            content: MessageContent::Text(prompt.to_string()),
        }];

        let mut total_input = 0u32;
        let mut total_output = 0u32;

        for turn in 0..self.max_turns {
            let request = MessagesRequest {
                model: &self.model,
                max_tokens: self.max_tokens,
                system: &self.system,
                tools: &self.tools,
                messages: &messages,
            };

            let response = self.client.create_message(&request).await?;
            total_input += response.usage.input_tokens;
            total_output += response.usage.output_tokens;

            let mut tool_uses: Vec<(String, String, serde_json::Value)> = Vec::new();
            let mut assistant_content: Vec<ContentBlock> = Vec::new();

            for block in &response.content {
                match block {
                    ContentBlock::Text { text } => {
                        assistant_content.push(ContentBlock::Text { text: text.clone() });
                        send(tx, AgentMessageEvent::Text { text: text.clone() }).await?;
                    }
                    ContentBlock::ToolUse { id, name, input } => {
                        assistant_content.push(ContentBlock::ToolUse {
                            id: id.clone(),
                            name: name.clone(),
                            input: input.clone(),
                        });
                        send(
                            tx,
                            AgentMessageEvent::ToolCall {
                                name: name.clone(),
                                arguments: Some(input.clone()),
                            },
                        )
                        .await?;
                        tool_uses.push((id.clone(), name.clone(), input.clone()));
                    }
                    ContentBlock::ToolResult { .. } => {
                        // ToolResult blocks are never returned by the API
                    }
                }
            }

            messages.push(Message::Assistant {
                content: assistant_content,
            });

            if response.stop_reason != Some(StopReason::ToolUse) || tool_uses.is_empty() {
                send(
                    tx,
                    AgentMessageEvent::Done {
                        turns: turn as u32 + 1,
                        input_tokens: total_input,
                        output_tokens: total_output,
                        duration_ms: None,
                        cost_usd: None,
                    },
                )
                .await?;
                return Ok(());
            }

            // Execute tools — route meta-tools through catalog methods
            let mut tool_results: Vec<ContentBlock> = Vec::new();
            for (id, name, input) in &tool_uses {
                let result = self.dispatch_tool(name, input).await;
                let (text, is_error) = match &result {
                    Ok(t) => (t.clone(), false),
                    Err(e) => (e.to_string(), true),
                };
                send(
                    tx,
                    AgentMessageEvent::ToolResult {
                        name: name.clone(),
                        is_error,
                    },
                )
                .await?;
                tool_results.push(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: text,
                    is_error: if is_error { Some(true) } else { None },
                });
            }

            messages.push(Message::User {
                content: MessageContent::Blocks(tool_results),
            });
        }

        Err(AgentError::MaxTurnsReached(self.max_turns))
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the light (in-process) agentic loop.
///
/// Returns a channel receiver that streams `AgentMessageEvent`s, matching the
/// same interface used by the sandbox runtime.
#[instrument(name = "agent.light.run", skip_all)]
pub(super) async fn run(
    prompt: String,
    config: &LightRuntimeConfig,
    chat_config: &ChatConfig,
    system_prompt: &str,
    catalog: Catalog,
) -> Result<mpsc::Receiver<AgentMessageEvent>, AgentError> {
    if config.api_key.is_empty() {
        return Err(AgentError::LightAgentNotConfigured);
    }

    let model = chat_config.model.clone();
    let max_tokens = chat_config.max_tokens;
    let max_turns = chat_config.max_turns as usize;

    let tools = meta_tool_definitions();
    let system = build_system_prompt(system_prompt, &catalog);

    let session = LightSession {
        client: AnthropicClient::new(config.api_key.clone()),
        catalog,
        model,
        max_tokens,
        max_turns,
        tools,
        system,
    };

    let (tx, rx) = mpsc::channel::<AgentMessageEvent>(64);

    tokio::spawn(async move {
        if let Err(e) = session.run(&prompt, &tx).await {
            tracing::error!(error = %e, "Light agent loop failed");
            let _ = tx
                .send(AgentMessageEvent::Error {
                    message: e.to_string(),
                })
                .await;
        }
    });

    Ok(rx)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn send(
    tx: &mpsc::Sender<AgentMessageEvent>,
    event: AgentMessageEvent,
) -> Result<(), AgentError> {
    tx.send(event)
        .await
        .map_err(|_| AgentError::ChannelClosed)?;
    Ok(())
}

fn build_system_prompt(base: &str, catalog: &Catalog) -> String {
    let instructions = catalog.instructions();
    if instructions.is_empty() {
        base.to_string()
    } else {
        format!("{base}\n\n{instructions}")
    }
}

fn call_result_to_text(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
