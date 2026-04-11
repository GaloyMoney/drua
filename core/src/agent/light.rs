use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::instrument;

use crate::chat_history::{ChatHistory, ConversationId};
use crate::toolset::Catalog;

use super::config::LightRuntimeConfig;
use super::entity::ChatConfig;
use super::error::AgentError;
use super::AgentMessageEvent;

// ---------------------------------------------------------------------------
// Anthropic API constants
// ---------------------------------------------------------------------------

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";
/// Beta headers required for server-side context management.
/// - compact-2026-01-12: enables compaction
/// - context-management-2025-06-27: enables tool result + thinking block clearing
const BETA_HEADERS: &str = "compact-2026-01-12,context-management-2025-06-27";

// ---------------------------------------------------------------------------
// Anthropic API response types
// ---------------------------------------------------------------------------

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
    /// Server-side compaction summary replacing older conversation history.
    Compaction {
        content: String,
    },
}

#[derive(Deserialize, Clone)]
struct ApiUsage {
    input_tokens: u32,
    output_tokens: u32,
}

// ---------------------------------------------------------------------------
// Context management config (request types)
// ---------------------------------------------------------------------------

/// Configuration for Anthropic's server-side context management.
///
/// Serializes to the `context_management` field in the Messages API request.
/// See: https://platform.claude.com/docs/en/build-with-claude/compaction
///      https://platform.claude.com/docs/en/build-with-claude/context-editing
#[derive(Clone, Serialize)]
struct ContextManagementConfig {
    edits: Vec<ContextManagementEdit>,
}

#[derive(Clone, Serialize)]
#[serde(tag = "type")]
enum ContextManagementEdit {
    /// Auto-compaction when input tokens exceed threshold.
    #[serde(rename = "compact_20260112")]
    Compact {
        trigger: TokenTrigger,
        #[serde(skip_serializing_if = "Option::is_none")]
        pause_after_compaction: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        instructions: Option<String>,
    },
    /// Clear oldest tool results when input tokens exceed threshold.
    #[serde(rename = "clear_tool_uses_20250919")]
    ClearToolUses {
        #[serde(skip_serializing_if = "Option::is_none")]
        trigger: Option<TokenTrigger>,
        #[serde(skip_serializing_if = "Option::is_none")]
        keep: Option<ToolUsesKeep>,
    },
    /// Manage extended thinking blocks in multi-turn conversations.
    #[serde(rename = "clear_thinking_20251015")]
    ClearThinking {},
}

#[derive(Clone, Serialize)]
struct TokenTrigger {
    #[serde(rename = "type")]
    kind: &'static str,
    value: u32,
}

impl TokenTrigger {
    fn input_tokens(value: u32) -> Self {
        Self {
            kind: "input_tokens",
            value,
        }
    }
}

#[derive(Clone, Serialize)]
struct ToolUsesKeep {
    #[serde(rename = "type")]
    kind: &'static str,
    value: u32,
}

impl ToolUsesKeep {
    fn tool_uses(value: u32) -> Self {
        Self {
            kind: "tool_uses",
            value,
        }
    }
}

/// Default context management configuration for light agents.
///
/// Enables all three server-side strategies:
/// - Compaction at 50K input tokens
/// - Tool result clearing at 30K tokens, keeping 4 most recent
/// - Thinking block clearing (default behavior)
fn default_context_management() -> ContextManagementConfig {
    ContextManagementConfig {
        edits: vec![
            ContextManagementEdit::Compact {
                trigger: TokenTrigger::input_tokens(50_000),
                pause_after_compaction: None,
                instructions: None,
            },
            ContextManagementEdit::ClearToolUses {
                trigger: Some(TokenTrigger::input_tokens(30_000)),
                keep: Some(ToolUsesKeep::tool_uses(4)),
            },
            ContextManagementEdit::ClearThinking {},
        ],
    }
}

// ---------------------------------------------------------------------------
// Anthropic HTTP client
// ---------------------------------------------------------------------------

struct AnthropicClient {
    http: reqwest::Client,
    api_key: String,
    beta_headers: Option<&'static str>,
}

impl AnthropicClient {
    fn new(api_key: String, beta_headers: Option<&'static str>) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key,
            beta_headers,
        }
    }

    async fn create_message(&self, body: serde_json::Value) -> Result<ApiResponse, AgentError> {
        let mut req = self
            .http
            .post(API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json");

        if let Some(beta) = self.beta_headers {
            req = req.header("anthropic-beta", beta);
        }

        let resp = req.json(&body).send().await.map_err(AgentError::Http)?;

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
    system: serde_json::Value,
    context_management: Option<ContextManagementConfig>,
    chat_history: ChatHistory,
    conversation_id: ConversationId,
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
        initial_messages: Vec<serde_json::Value>,
        tx: &mpsc::Sender<AgentMessageEvent>,
    ) -> Result<(), AgentError> {
        let mut messages = initial_messages;

        let mut total_input = 0u32;
        let mut total_output = 0u32;

        for turn in 0..self.max_turns {
            let mut body = serde_json::json!({
                "model": self.model,
                "max_tokens": self.max_tokens,
                "system": self.system,
                "tools": self.tools,
                "messages": messages,
            });

            if let Some(ctx_mgmt) = &self.context_management {
                if let Ok(v) = serde_json::to_value(ctx_mgmt) {
                    body["context_management"] = v;
                }
            }

            let response = self.client.create_message(body).await?;
            total_input += response.usage.input_tokens;
            total_output += response.usage.output_tokens;

            let mut tool_uses: Vec<(String, String, serde_json::Value)> = Vec::new();
            let mut assistant_content: Vec<serde_json::Value> = Vec::new();
            let mut had_compaction = false;

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
                    ApiContentBlock::Compaction { content } => {
                        // Add cache_control to compaction blocks for prompt caching.
                        // The API drops all content before the compaction block on
                        // subsequent requests, so caching it reduces re-processing.
                        assistant_content.push(serde_json::json!({
                            "type": "compaction",
                            "content": content,
                            "cache_control": {"type": "ephemeral"}
                        }));
                        had_compaction = true;
                        tracing::info!(summary_len = content.len(), "Context compaction occurred");
                    }
                }
            }

            messages.push(serde_json::json!({
                "role": "assistant", "content": assistant_content
            }));

            let stop = response.stop_reason.as_deref().unwrap_or("end_turn");

            // Handle pause_after_compaction: the API returns stop_reason "compaction"
            // with only the compaction block. We continue the loop to get the actual
            // response on the next turn.
            if stop == "compaction" {
                tracing::info!("Compaction pause — continuing conversation loop");
                continue;
            }

            if stop != "tool_use" || tool_uses.is_empty() {
                // Persist the full API messages array for conversation resume
                self.persist_api_messages(&messages).await;

                if had_compaction {
                    send(
                        tx,
                        AgentMessageEvent::Service {
                            message: "Context compacted — conversation history summarized"
                                .to_string(),
                        },
                    )
                    .await?;
                }

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

            // Persist api_messages after each tool turn for crash recovery
            self.persist_api_messages(&messages).await;
        }

        // Persist on max turns too
        self.persist_api_messages(&messages).await;
        Err(AgentError::MaxTurnsReached(self.max_turns))
    }

    /// Persist the full Anthropic API messages array for conversation resume.
    async fn persist_api_messages(&self, messages: &[serde_json::Value]) {
        let json = serde_json::Value::Array(messages.to_vec());
        if let Err(e) = self
            .chat_history
            .update_api_messages(self.conversation_id, &json)
            .await
        {
            tracing::warn!(error = %e, "Failed to persist api_messages");
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Parameters for running the light agent loop.
pub(super) struct LightRunParams {
    pub prompt: String,
    pub catalog: Catalog,
    pub chat_history: ChatHistory,
    pub conversation_id: ConversationId,
    /// Prior API messages to resume from (conversation continuation).
    pub prior_messages: Option<Vec<serde_json::Value>>,
}

/// Run the light (in-process) agentic loop.
///
/// Returns a channel receiver that streams `AgentMessageEvent`s, matching the
/// same interface used by the sandbox runtime.
///
/// When `prior_messages` is provided (conversation resume), the prompt is
/// appended to the existing message history. Otherwise a fresh conversation
/// starts.
#[instrument(name = "agent.light.run", skip_all)]
pub(super) async fn run(
    config: &LightRuntimeConfig,
    chat_config: &ChatConfig,
    system_prompt: &str,
    params: LightRunParams,
) -> Result<mpsc::Receiver<AgentMessageEvent>, AgentError> {
    if config.api_key.is_empty() {
        return Err(AgentError::LightAgentNotConfigured);
    }

    let LightRunParams {
        prompt,
        catalog,
        chat_history,
        conversation_id,
        prior_messages,
    } = params;

    let model = chat_config.model.clone();
    let max_tokens = chat_config.max_tokens;
    let max_turns = chat_config.max_turns as usize;

    let tools = Catalog::meta_tool_definitions();
    let system_text = build_system_prompt(system_prompt, &catalog);

    // Build system prompt with cache_control for prompt caching.
    // The structured format enables Anthropic's automatic caching of the
    // system prompt across requests (5-minute TTL).
    let system = serde_json::json!([{
        "type": "text",
        "text": system_text,
        "cache_control": {"type": "ephemeral"}
    }]);

    let context_management = Some(default_context_management());

    // Build initial messages: resume from prior state or start fresh
    let initial_messages = match prior_messages {
        Some(mut msgs) => {
            msgs.push(serde_json::json!({ "role": "user", "content": prompt }));
            msgs
        }
        None => {
            vec![serde_json::json!({ "role": "user", "content": prompt })]
        }
    };

    let session = LightSession {
        client: AnthropicClient::new(config.api_key.clone(), Some(BETA_HEADERS)),
        catalog,
        model,
        max_tokens,
        max_turns,
        tools,
        system,
        context_management,
        chat_history,
        conversation_id,
    };

    let (tx, rx) = mpsc::channel::<AgentMessageEvent>(64);

    tokio::spawn(async move {
        if let Err(e) = session.run(initial_messages, &tx).await {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_management_config_serializes_correctly() {
        let config = default_context_management();
        let json = serde_json::to_value(&config).unwrap();

        let edits = json["edits"].as_array().unwrap();
        assert_eq!(edits.len(), 3);

        // Compact edit
        assert_eq!(edits[0]["type"], "compact_20260112");
        assert_eq!(edits[0]["trigger"]["type"], "input_tokens");
        assert_eq!(edits[0]["trigger"]["value"], 50_000);
        assert!(edits[0].get("pause_after_compaction").is_none());

        // Clear tool uses edit
        assert_eq!(edits[1]["type"], "clear_tool_uses_20250919");
        assert_eq!(edits[1]["trigger"]["type"], "input_tokens");
        assert_eq!(edits[1]["trigger"]["value"], 30_000);
        assert_eq!(edits[1]["keep"]["type"], "tool_uses");
        assert_eq!(edits[1]["keep"]["value"], 4);

        // Clear thinking edit
        assert_eq!(edits[2]["type"], "clear_thinking_20251015");
    }

    #[test]
    fn compaction_block_deserializes() {
        let json = serde_json::json!({
            "type": "compaction",
            "content": "Summary of conversation so far..."
        });
        let block: ApiContentBlock = serde_json::from_value(json).unwrap();
        match block {
            ApiContentBlock::Compaction { content } => {
                assert_eq!(content, "Summary of conversation so far...");
            }
            _ => panic!("Expected Compaction variant"),
        }
    }

    #[test]
    fn api_response_with_compaction_deserializes() {
        let json = serde_json::json!({
            "content": [
                {"type": "compaction", "content": "Summary..."},
                {"type": "text", "text": "Based on our conversation..."}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 23000, "output_tokens": 1000}
        });
        let response: ApiResponse = serde_json::from_value(json).unwrap();
        assert_eq!(response.content.len(), 2);
        assert!(matches!(
            &response.content[0],
            ApiContentBlock::Compaction { .. }
        ));
        assert!(matches!(&response.content[1], ApiContentBlock::Text { .. }));
        assert_eq!(response.stop_reason.as_deref(), Some("end_turn"));
    }

    #[test]
    fn api_response_compaction_stop_reason() {
        let json = serde_json::json!({
            "content": [
                {"type": "compaction", "content": "Summary..."}
            ],
            "stop_reason": "compaction",
            "usage": {"input_tokens": 180000, "output_tokens": 3500}
        });
        let response: ApiResponse = serde_json::from_value(json).unwrap();
        assert_eq!(response.stop_reason.as_deref(), Some("compaction"));
        assert_eq!(response.content.len(), 1);
    }
}
