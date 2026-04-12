use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::instrument;

use crate::toolset::Catalog;

use super::config::LightRuntimeConfig;
use super::entity::ChatConfig;
use super::error::AgentError;
use super::AgentMessageEvent;

// ---------------------------------------------------------------------------
// OpenAI Chat Completions API types
// ---------------------------------------------------------------------------

const API_URL: &str = "https://api.openai.com/v1/chat/completions";

#[derive(Deserialize)]
struct ApiResponse {
    choices: Vec<ApiChoice>,
    usage: ApiUsage,
}

#[derive(Deserialize)]
struct ApiChoice {
    message: ApiMessage,
    finish_reason: Option<String>,
}

#[derive(Deserialize, Clone)]
struct ApiMessage {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ApiToolCall>>,
}

#[derive(Deserialize, Clone)]
struct ApiToolCall {
    id: String,
    function: ApiFunction,
}

#[derive(Deserialize, Clone)]
struct ApiFunction {
    name: String,
    arguments: String, // JSON string — must serde_json::from_str()
}

#[derive(Deserialize, Clone)]
struct ApiUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
}

// ---------------------------------------------------------------------------
// OpenAI HTTP client
// ---------------------------------------------------------------------------

struct OpenaiClient {
    http: reqwest::Client,
    api_key: String,
}

impl OpenaiClient {
    fn new(api_key: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key,
        }
    }

    async fn create_message(&self, body: serde_json::Value) -> Result<ApiResponse, AgentError> {
        let resp = self
            .http
            .post(API_URL)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(AgentError::Http)?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(AgentError::OpenaiApi {
                status: status.as_u16(),
                message: text,
            });
        }

        resp.json().await.map_err(AgentError::Http)
    }
}

// ---------------------------------------------------------------------------
// OpenAI light agent session
// ---------------------------------------------------------------------------

struct OpenaiLightSession {
    client: OpenaiClient,
    catalog: Catalog,
    model: String,
    max_tokens: u32,
    max_turns: usize,
    tools: Vec<serde_json::Value>,
    system: String,
}

impl OpenaiLightSession {
    /// Route a tool call to the appropriate handler (same dispatch as Anthropic).
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
        // OpenAI: system prompt is a message in the array (not a top-level field)
        let mut messages: Vec<serde_json::Value> = vec![
            serde_json::json!({ "role": "system", "content": self.system }),
            serde_json::json!({ "role": "user", "content": prompt }),
        ];

        let mut total_input = 0u32;
        let mut total_output = 0u32;

        for turn in 0..self.max_turns {
            let body = serde_json::json!({
                "model": self.model,
                "max_tokens": self.max_tokens,
                "tools": self.tools,
                "messages": messages,
            });

            let response = self.client.create_message(body).await?;
            total_input += response.usage.prompt_tokens;
            total_output += response.usage.completion_tokens;

            let choice =
                response
                    .choices
                    .into_iter()
                    .next()
                    .ok_or_else(|| AgentError::OpenaiApi {
                        status: 0,
                        message: "No choices in OpenAI response".to_string(),
                    })?;

            // Emit text content if present
            if let Some(ref text) = choice.message.content {
                if !text.is_empty() {
                    send(tx, AgentMessageEvent::Text { text: text.clone() }).await?;
                }
            }

            let tool_calls = choice.message.tool_calls.clone().unwrap_or_default();

            // Emit tool call events (parse arguments from JSON string)
            for tc in &tool_calls {
                let args: serde_json::Value =
                    serde_json::from_str(&tc.function.arguments).unwrap_or_default();
                send(
                    tx,
                    AgentMessageEvent::ToolCall {
                        name: tc.function.name.clone(),
                        arguments: Some(args),
                    },
                )
                .await?;
            }

            // Append assistant message to conversation history.
            // Must include tool_calls verbatim so the API can match tool results.
            let mut assistant_msg = serde_json::json!({ "role": "assistant" });
            assistant_msg["content"] = match &choice.message.content {
                Some(text) => serde_json::json!(text),
                None => serde_json::Value::Null,
            };
            if !tool_calls.is_empty() {
                let tc_json: Vec<serde_json::Value> = tool_calls
                    .iter()
                    .map(|tc| {
                        serde_json::json!({
                            "id": tc.id,
                            "type": "function",
                            "function": {
                                "name": tc.function.name,
                                "arguments": tc.function.arguments,
                            }
                        })
                    })
                    .collect();
                assistant_msg["tool_calls"] = serde_json::json!(tc_json);
            }
            messages.push(assistant_msg);

            // OpenAI: finish_reason "stop" = done, "tool_calls" = execute tools
            let finish = choice.finish_reason.as_deref().unwrap_or("stop");
            if finish != "tool_calls" || tool_calls.is_empty() {
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

            // Execute tools and send results back as separate role:"tool" messages
            for tc in &tool_calls {
                let args: serde_json::Value =
                    serde_json::from_str(&tc.function.arguments).unwrap_or_default();
                let result = self.dispatch_tool(&tc.function.name, &args).await;
                let (text, is_error) = match &result {
                    Ok(t) => (t.clone(), false),
                    Err(e) => (e.to_string(), true),
                };
                send(
                    tx,
                    AgentMessageEvent::ToolResult {
                        name: tc.function.name.clone(),
                        is_error,
                    },
                )
                .await?;

                // OpenAI: each tool result is a separate message with role: "tool"
                messages.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": tc.id,
                    "content": text,
                }));
            }
        }

        Err(AgentError::MaxTurnsReached(self.max_turns))
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the OpenAI-backed light (in-process) agentic loop.
///
/// Same interface as `light::run` — returns a channel receiver that streams
/// `AgentMessageEvent`s. Uses OpenAI Chat Completions API instead of Anthropic.
#[instrument(name = "agent.openai_light.run", skip_all)]
pub(super) async fn run(
    prompt: String,
    config: &LightRuntimeConfig,
    chat_config: &ChatConfig,
    system_prompt: &str,
    catalog: Catalog,
) -> Result<mpsc::Receiver<AgentMessageEvent>, AgentError> {
    if config.openai_api_key.is_empty() {
        return Err(AgentError::LightAgentNotConfigured);
    }

    // Use configured model or default to gpt-4.1
    let model = if config.openai_model.is_empty() {
        "gpt-4.1".to_string()
    } else {
        config.openai_model.clone()
    };
    let max_tokens = chat_config.max_tokens;
    let max_turns = chat_config.max_turns as usize;

    // Convert Anthropic-format tool definitions to OpenAI function calling format
    let tools = convert_tools_for_openai(&Catalog::meta_tool_definitions());
    let system = build_system_prompt(system_prompt, &catalog);

    let session = OpenaiLightSession {
        client: OpenaiClient::new(config.openai_api_key.clone()),
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
            tracing::error!(error = %e, "OpenAI light agent loop failed");
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

/// Convert Anthropic-format tool definitions to OpenAI function calling format.
///
/// Anthropic: `{name, description, input_schema: {...}}`
/// OpenAI:    `{type: "function", function: {name, description, parameters: {...}}}`
fn convert_tools_for_openai(anthropic_tools: &[serde_json::Value]) -> Vec<serde_json::Value> {
    anthropic_tools
        .iter()
        .map(|tool| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": tool["name"],
                    "description": tool["description"],
                    "parameters": tool["input_schema"],
                }
            })
        })
        .collect()
}
