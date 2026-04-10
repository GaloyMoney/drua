use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::instrument;

use crate::toolset::Catalog;

use super::config::LightRuntimeConfig;
use super::entity::ChatConfig;
use super::error::AgentError;
use super::AgentMessageEvent;

// ---------------------------------------------------------------------------
// Anthropic API types
// ---------------------------------------------------------------------------

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";

#[derive(Deserialize)]
struct ApiResponse {
    content: Vec<ApiContentBlock>,
    stop_reason: Option<String>,
    usage: ApiUsage,
}

#[derive(Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ApiContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

#[derive(Deserialize, Clone)]
struct ApiUsage {
    input_tokens: u32,
    output_tokens: u32,
}

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

    async fn create_message(&self, body: serde_json::Value) -> Result<ApiResponse, AgentError> {
        let resp = self
            .http
            .post(API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json")
            .json(&body)
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
// Light agent session (groups agentic loop state)
// ---------------------------------------------------------------------------

struct LightSession {
    client: AnthropicClient,
    catalog: Catalog,
    model: String,
    max_tokens: u32,
    max_turns: usize,
    tools: Vec<serde_json::Value>,
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
        let mut messages: Vec<serde_json::Value> =
            vec![serde_json::json!({ "role": "user", "content": prompt })];

        let mut total_input = 0u32;
        let mut total_output = 0u32;

        for turn in 0..self.max_turns {
            let body = serde_json::json!({
                "model": self.model,
                "max_tokens": self.max_tokens,
                "system": self.system,
                "tools": self.tools,
                "messages": messages,
            });

            let response = self.client.create_message(body).await?;
            total_input += response.usage.input_tokens;
            total_output += response.usage.output_tokens;

            let mut tool_uses: Vec<(String, String, serde_json::Value)> = Vec::new();
            let mut assistant_content: Vec<serde_json::Value> = Vec::new();

            for block in &response.content {
                match block {
                    ApiContentBlock::Text { text } => {
                        assistant_content.push(serde_json::json!({"type": "text", "text": text}));
                        send(tx, AgentMessageEvent::Text { text: text.clone() }).await?;
                    }
                    ApiContentBlock::ToolUse { id, name, input } => {
                        assistant_content.push(serde_json::json!({
                            "type": "tool_use", "id": id, "name": name, "input": input
                        }));
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
                }
            }

            messages.push(serde_json::json!({
                "role": "assistant", "content": assistant_content
            }));

            let stop = response.stop_reason.as_deref().unwrap_or("end_turn");
            if stop != "tool_use" || tool_uses.is_empty() {
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
            let mut tool_results: Vec<serde_json::Value> = Vec::new();
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
                let mut result_json = serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": text,
                });
                if is_error {
                    result_json["is_error"] = serde_json::json!(true);
                }
                tool_results.push(result_json);
            }

            messages.push(serde_json::json!({"role": "user", "content": tool_results}));
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

    let tools = Catalog::meta_tool_definitions();
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
