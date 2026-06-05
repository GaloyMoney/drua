//! Adapters between the provider-agnostic `llm` types and OpenAI wire types.

use llm::prompt::{
    AssistantBlock, Message, SystemBlock, Tool, ToolChoice, ToolResultBlock, UserBlock,
};
use llm::stream::StreamDelta;
use llm::StopReason;

use crate::types::{
    OpenAiCacheControl, OpenAiContentBlock, OpenAiFunction, OpenAiMessage, OpenAiMessageContent,
    OpenAiRequest, OpenAiRequestToolCall, OpenAiStreamChunk, OpenAiTool, OpenAiToolChoice,
    OpenAiToolChoiceFunction, OpenAiToolFunction, ReasoningConfig, StreamOptions,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReasoningDialect {
    OpenAi,
    OpenRouter,
}

fn reasoning_effort_str(effort: llm::ReasoningEffort) -> &'static str {
    match effort {
        llm::ReasoningEffort::Low => "low",
        llm::ReasoningEffort::Medium => "medium",
        llm::ReasoningEffort::High => "high",
        llm::ReasoningEffort::XHigh => "xhigh",
    }
}

/// OpenRouter routes any model id starting with `anthropic/` to Anthropic
/// upstream, where prompt caching only engages on requests carrying explicit
/// `cache_control` markers. The OpenAI Chat Completions request shape carries
/// these as a passthrough field.
const ANTHROPIC_MODEL_PREFIX: &str = "anthropic/";

pub(crate) fn prompt_to_request(
    prompt: &llm::Prompt,
    reasoning_dialect: ReasoningDialect,
) -> OpenAiRequest {
    let mut messages: Vec<OpenAiMessage> = Vec::new();

    for block in &prompt.system {
        match block {
            SystemBlock::Text { text, .. } => {
                messages.push(OpenAiMessage {
                    role: "system",
                    content: Some(OpenAiMessageContent::Text(text.clone())),
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
        }
    }

    for msg in &prompt.messages {
        convert_message(msg, &mut messages);
    }

    let tools = if prompt.tools.is_empty() {
        None
    } else {
        Some(prompt.tools.iter().map(convert_tool).collect())
    };

    let tool_choice = prompt.tool_choice.as_ref().map(convert_tool_choice);

    let effort = reasoning_effort_str(prompt.effort);
    let mut request = OpenAiRequest {
        model: prompt.chain.primary.name.clone(),
        messages,
        prompt_cache_key: prompt.cache_key.clone(),
        max_completion_tokens: prompt.max_tokens,
        temperature: None,
        tools,
        tool_choice,
        stream: true,
        stream_options: Some(StreamOptions {
            include_usage: true,
        }),
        reasoning_effort: match reasoning_dialect {
            ReasoningDialect::OpenAi => Some(effort),
            ReasoningDialect::OpenRouter => None,
        },
        reasoning: match reasoning_dialect {
            ReasoningDialect::OpenAi => None,
            ReasoningDialect::OpenRouter => Some(ReasoningConfig { effort }),
        },
    };

    if request.model.starts_with(ANTHROPIC_MODEL_PREFIX) {
        apply_anthropic_prompt_caching(&mut request);
    }

    request
}

/// Stamps a single `{type:"ephemeral"}` marker on the highest-value cacheable
/// block: last user/system message → last tool definition. Anthropic
/// auto-checks earlier breakpoints, so one marker covers the prefix. Mirrors
/// `lib/anthropic-client/src/convert.rs::apply_prompt_caching`.
fn apply_anthropic_prompt_caching(req: &mut OpenAiRequest) {
    let marker = OpenAiCacheControl {
        r#type: "ephemeral",
        ttl: None,
    };

    if let Some(last_msg) = req.messages.last_mut() {
        if mark_message(last_msg, &marker) {
            return;
        }
    }
    if let Some(tool) = req.tools.as_mut().and_then(|v| v.last_mut()) {
        tool.cache_control = Some(marker);
    }
}

/// Promotes the message content to array-of-blocks shape (required by the
/// Chat Completions caching extension) and marks the last text block.
/// Returns false when the message has no text content (pure tool_calls).
fn mark_message(msg: &mut OpenAiMessage, marker: &OpenAiCacheControl) -> bool {
    let text = match msg.content.take() {
        Some(OpenAiMessageContent::Text(t)) => t,
        Some(OpenAiMessageContent::Blocks(mut blocks)) => {
            if let Some(block) = blocks.last_mut() {
                block.cache_control = Some(marker.clone());
                msg.content = Some(OpenAiMessageContent::Blocks(blocks));
                return true;
            }
            msg.content = Some(OpenAiMessageContent::Blocks(blocks));
            return false;
        }
        None => return false,
    };

    msg.content = Some(OpenAiMessageContent::Blocks(vec![OpenAiContentBlock {
        r#type: "text",
        text,
        cache_control: Some(marker.clone()),
    }]));
    true
}

fn convert_message(message: &Message, out: &mut Vec<OpenAiMessage>) {
    match message {
        Message::User { content } => {
            // Merge consecutive text blocks into a single user message; tool
            // results become separate "tool" role messages.
            let mut text_parts: Vec<String> = Vec::new();

            for block in content {
                match block {
                    UserBlock::Text { text, .. } => {
                        text_parts.push(text.clone());
                    }
                    UserBlock::ToolResult {
                        tool_use_id,
                        content: result_content,
                        ..
                    } => {
                        if !text_parts.is_empty() {
                            out.push(OpenAiMessage {
                                role: "user",
                                content: Some(OpenAiMessageContent::Text(text_parts.join("\n"))),
                                tool_calls: None,
                                tool_call_id: None,
                            });
                            text_parts.clear();
                        }
                        let text = result_content
                            .iter()
                            .map(|b| match b {
                                ToolResultBlock::Text { text } => text.as_str(),
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        out.push(OpenAiMessage {
                            role: "tool",
                            content: Some(OpenAiMessageContent::Text(text)),
                            tool_calls: None,
                            tool_call_id: Some(tool_use_id.clone()),
                        });
                    }
                }
            }
            if !text_parts.is_empty() {
                out.push(OpenAiMessage {
                    role: "user",
                    content: Some(OpenAiMessageContent::Text(text_parts.join("\n"))),
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
        }
        Message::Assistant { content } => {
            let mut text_parts: Vec<String> = Vec::new();
            let mut tool_calls: Vec<OpenAiRequestToolCall> = Vec::new();

            for block in content {
                match block {
                    AssistantBlock::Text { text, .. } => {
                        text_parts.push(text.clone());
                    }
                    AssistantBlock::ToolUse {
                        id, name, input, ..
                    } => {
                        tool_calls.push(OpenAiRequestToolCall {
                            id: id.clone(),
                            r#type: "function",
                            function: OpenAiFunction {
                                name: name.clone(),
                                arguments: serde_json::to_string(input).unwrap_or_default(),
                            },
                        });
                    }
                    AssistantBlock::Thinking { .. } => {
                        // Anthropic-specific; dropped for OpenAI.
                    }
                }
            }

            let content = if text_parts.is_empty() {
                None
            } else {
                Some(OpenAiMessageContent::Text(text_parts.join("\n")))
            };

            let tool_calls_field = if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            };

            out.push(OpenAiMessage {
                role: "assistant",
                content,
                tool_calls: tool_calls_field,
                tool_call_id: None,
            });
        }
    }
}

fn convert_tool(tool: &Tool) -> OpenAiTool {
    OpenAiTool {
        r#type: "function",
        function: OpenAiToolFunction {
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: tool.input_schema.clone(),
            strict: if tool.strict { Some(true) } else { None },
        },
        cache_control: None,
    }
}

fn convert_tool_choice(choice: &ToolChoice) -> OpenAiToolChoice {
    match choice {
        ToolChoice::Auto => OpenAiToolChoice::String("auto"),
        ToolChoice::Any => OpenAiToolChoice::String("required"),
        ToolChoice::None => OpenAiToolChoice::String("none"),
        ToolChoice::Tool { name } => OpenAiToolChoice::Specific {
            r#type: "function",
            function: OpenAiToolChoiceFunction { name: name.clone() },
        },
    }
}

/// Sentinel error string returned by `DeltaSynthesizer::process_chunk` when the
/// upstream stream finishes with `finish_reason="stop"` having emitted neither
/// text nor tool calls. `OpenAiClient` matches on this to retry once.
pub(crate) const EMPTY_COMPLETION_ERR: &str =
    "upstream returned empty completion (no text, no tool calls) with finish_reason=stop";

/// Converts OpenAI streaming chunks into provider-agnostic [`StreamDelta`]s.
/// Tracks tool call IDs by index so subsequent argument deltas can reference them.
///
/// Usage emission is deferred until either `finish_reason` or the `[DONE]`
/// sentinel arrives. This way an empty-completion error (returned at
/// `finish_reason="stop"` with no text/tool deltas) discards the stashed
/// usage and nothing observable has been emitted, leaving the caller free
/// to retry the request without corrupting the consumer's accumulator.
pub(crate) struct DeltaSynthesizer {
    tool_ids: Vec<Option<String>>,
    saw_text: bool,
    saw_tool_call: bool,
    pending_usage: Option<PendingUsage>,
}

#[derive(Copy, Clone)]
struct PendingUsage {
    input_tokens: u32,
    output_tokens: u32,
    cache_read_input_tokens: u32,
    cache_creation_input_tokens: u32,
    reasoning_output_tokens: u32,
    cost_usd: Option<f64>,
    upstream_inference_cost_usd: Option<f64>,
}

impl DeltaSynthesizer {
    pub fn new() -> Self {
        Self {
            tool_ids: Vec::new(),
            saw_text: false,
            saw_tool_call: false,
            pending_usage: None,
        }
    }

    pub fn process_chunk(&mut self, data: &str) -> Result<Vec<StreamDelta>, String> {
        if data.trim() == "[DONE]" {
            // Flush any usage that arrived in a separate post-finish chunk
            // (some providers split usage into its own trailing chunk).
            return Ok(self.drain_pending_usage());
        }

        let chunk: OpenAiStreamChunk =
            serde_json::from_str(data).map_err(|e| format!("JSON parse: {e}"))?;

        let mut deltas = Vec::new();

        if let Some(usage) = &chunk.usage {
            let (cache_read, cache_write) = usage
                .prompt_tokens_details
                .as_ref()
                .map(|d| (d.cached_tokens, d.cache_write_tokens))
                .unwrap_or((0, 0));
            let reasoning = usage
                .completion_tokens_details
                .as_ref()
                .map(|d| d.reasoning_tokens)
                .unwrap_or(0);
            let upstream = usage
                .cost_details
                .as_ref()
                .and_then(|d| d.upstream_inference_cost);
            self.pending_usage = Some(PendingUsage {
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
                cache_read_input_tokens: cache_read,
                cache_creation_input_tokens: cache_write,
                reasoning_output_tokens: reasoning,
                cost_usd: usage.cost,
                upstream_inference_cost_usd: upstream,
            });
        }

        let Some(choice) = chunk.choices.first() else {
            return Ok(deltas);
        };

        if let Some(delta) = &choice.delta {
            if let Some(text) = &delta.content {
                if !text.is_empty() {
                    self.saw_text = true;
                    deltas.push(StreamDelta::TextDelta { text: text.clone() });
                }
            }

            if let Some(tool_calls) = &delta.tool_calls {
                for tc in tool_calls {
                    while self.tool_ids.len() <= tc.index {
                        self.tool_ids.push(None);
                    }

                    if self.tool_ids[tc.index].is_none() {
                        let id = tc.id.clone().unwrap_or_default();
                        let name = tc
                            .function
                            .as_ref()
                            .and_then(|f| f.name.clone())
                            .unwrap_or_default();
                        self.tool_ids[tc.index] = Some(id.clone());
                        self.saw_tool_call = true;
                        deltas.push(StreamDelta::ToolCallStart { id, name });
                    }

                    if let Some(func) = &tc.function {
                        if let Some(args) = &func.arguments {
                            if !args.is_empty() {
                                let id = self.tool_ids[tc.index].clone().unwrap_or_default();
                                deltas.push(StreamDelta::ToolCallDelta {
                                    id,
                                    partial_json: args.clone(),
                                });
                            }
                        }
                    }
                }
            }
        }

        if let Some(reason) = &choice.finish_reason {
            // An upstream "stop" with no text and no tool calls means the
            // model returned an empty completion. Some providers (e.g. deepseek
            // via OpenRouter) do this after a tool error and leave the workflow
            // executor to mistake the empty turn for a clean end_turn. Fail
            // the stream so the client can retry once before surfacing the
            // error instead of silently marking the turn end_turn.
            //
            // Deferred `pending_usage` is left on `self` and dropped with the
            // synthesizer — it never reaches the consumer.
            if reason == "stop" && !self.saw_text && !self.saw_tool_call {
                return Err(EMPTY_COMPLETION_ERR.to_string());
            }
            deltas.extend(self.drain_pending_usage());
            let stop_reason = match reason.as_str() {
                "stop" => Some(StopReason::EndTurn),
                "length" => Some(StopReason::MaxTokens),
                "tool_calls" => Some(StopReason::ToolUse),
                _ => None,
            };
            deltas.push(StreamDelta::Done { stop_reason });
        }

        Ok(deltas)
    }

    fn drain_pending_usage(&mut self) -> Vec<StreamDelta> {
        match self.pending_usage.take() {
            Some(u) => vec![StreamDelta::Usage {
                input_tokens: u.input_tokens,
                output_tokens: u.output_tokens,
                cache_read_input_tokens: u.cache_read_input_tokens,
                cache_creation_input_tokens: u.cache_creation_input_tokens,
                reasoning_output_tokens: u.reasoning_output_tokens,
                cost_usd: u.cost_usd,
                upstream_inference_cost_usd: u.upstream_inference_cost_usd,
            }],
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content_text(msg: &OpenAiMessage) -> Option<String> {
        match msg.content.as_ref()? {
            OpenAiMessageContent::Text(t) => Some(t.clone()),
            OpenAiMessageContent::Blocks(blocks) => Some(
                blocks
                    .iter()
                    .map(|b| b.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        }
    }

    fn cache_marker(block: &OpenAiContentBlock) -> Option<&OpenAiCacheControl> {
        block.cache_control.as_ref()
    }

    fn sample_prompt() -> llm::Prompt {
        llm::Prompt {
            chain: llm::ModelChain::new("gpt-4o"),
            effort: llm::ReasoningEffort::Low,
            system: vec![SystemBlock::Text {
                text: "You are a helpful assistant.".to_string(),
            }],
            messages: vec![Message::User {
                content: vec![UserBlock::Text {
                    text: "Hello".to_string(),
                }],
            }],
            tools: vec![Tool {
                name: "get_weather".to_string(),
                description: Some("Get weather".to_string()),
                input_schema: serde_json::json!({"type": "object", "properties": {"location": {"type": "string"}}}),
                strict: false,
            }],
            tool_choice: None,
            max_tokens: Some(1024),
            cache_key: None,
        }
    }

    #[test]
    fn reasoning_effort_serialized_when_set() {
        let mut prompt = sample_prompt();
        prompt.effort = llm::ReasoningEffort::High;
        let req = prompt_to_request(&prompt, ReasoningDialect::OpenAi);
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["reasoning_effort"], "high");
        assert!(json.get("reasoning").is_none());
    }

    #[test]
    fn openrouter_reasoning_serialized_when_set() {
        let mut prompt = sample_prompt();
        prompt.effort = llm::ReasoningEffort::High;
        let req = prompt_to_request(&prompt, ReasoningDialect::OpenRouter);
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["reasoning"]["effort"], "high");
        assert!(json.get("reasoning_effort").is_none());
    }

    #[test]
    fn low_reasoning_serialized_by_default() {
        let req = prompt_to_request(&sample_prompt(), ReasoningDialect::OpenAi);
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("reasoning").is_none());
        assert_eq!(json["reasoning_effort"], "low");
    }

    #[test]
    fn prompt_to_request_basic() {
        let prompt = sample_prompt();
        let req = prompt_to_request(&prompt, ReasoningDialect::OpenAi);

        assert_eq!(req.model, "gpt-4o");
        assert!(req.stream);
        assert_eq!(req.messages.len(), 2); // system + user
        assert_eq!(req.messages[0].role, "system");
        assert_eq!(req.messages[1].role, "user");
        assert_eq!(req.max_completion_tokens, Some(1024));
        assert!(req.prompt_cache_key.is_none());
        assert!(req.tools.is_some());
        assert_eq!(req.tools.as_ref().unwrap().len(), 1);
        assert_eq!(req.tools.as_ref().unwrap()[0].function.name, "get_weather");
    }

    #[test]
    fn prompt_to_request_includes_prompt_cache_key() {
        let mut prompt = sample_prompt();
        prompt.cache_key = Some("agent-session:test".to_string());

        let req = prompt_to_request(&prompt, ReasoningDialect::OpenAi);
        assert_eq!(req.prompt_cache_key.as_deref(), Some("agent-session:test"));
    }

    #[test]
    fn prompt_with_tool_results() {
        let prompt = llm::Prompt {
            chain: llm::ModelChain::new("gpt-4o"),
            effort: llm::ReasoningEffort::Low,
            system: vec![],
            messages: vec![
                Message::Assistant {
                    content: vec![AssistantBlock::ToolUse {
                        id: "call_123".to_string(),
                        name: "get_weather".to_string(),
                        input: serde_json::json!({"location": "NYC"}),
                    }],
                },
                Message::User {
                    content: vec![UserBlock::ToolResult {
                        tool_use_id: "call_123".to_string(),
                        content: vec![ToolResultBlock::Text {
                            text: "72F".to_string(),
                        }],
                        is_error: false,
                    }],
                },
            ],
            tools: vec![],
            tool_choice: None,
            max_tokens: None,
            cache_key: None,
        };

        let req = prompt_to_request(&prompt, ReasoningDialect::OpenAi);
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[0].role, "assistant");
        assert!(req.messages[0].tool_calls.is_some());
        assert_eq!(req.messages[1].role, "tool");
        assert_eq!(req.messages[1].tool_call_id.as_deref(), Some("call_123"));
        assert_eq!(content_text(&req.messages[1]), Some("72F".to_string()));
    }

    #[test]
    fn thinking_blocks_dropped() {
        let prompt = llm::Prompt {
            chain: llm::ModelChain::new("gpt-4o"),
            effort: llm::ReasoningEffort::Low,
            system: vec![],
            messages: vec![Message::Assistant {
                content: vec![
                    AssistantBlock::Thinking {
                        text: "internal reasoning".to_string(),
                        signature: Some("sig".to_string()),
                    },
                    AssistantBlock::Text {
                        text: "visible response".to_string(),
                    },
                ],
            }],
            tools: vec![],
            tool_choice: None,
            max_tokens: None,
            cache_key: None,
        };

        let req = prompt_to_request(&prompt, ReasoningDialect::OpenAi);
        assert_eq!(req.messages.len(), 1);
        assert_eq!(
            content_text(&req.messages[0]),
            Some("visible response".to_string())
        );
        assert!(req.messages[0].tool_calls.is_none());
    }

    #[test]
    fn tool_choice_mapping() {
        assert!(matches!(
            convert_tool_choice(&ToolChoice::Auto),
            OpenAiToolChoice::String("auto")
        ));
        assert!(matches!(
            convert_tool_choice(&ToolChoice::Any),
            OpenAiToolChoice::String("required")
        ));
        assert!(matches!(
            convert_tool_choice(&ToolChoice::None),
            OpenAiToolChoice::String("none")
        ));
        assert!(matches!(
            convert_tool_choice(&ToolChoice::Tool {
                name: "foo".to_string()
            }),
            OpenAiToolChoice::Specific { .. }
        ));
    }

    #[test]
    fn synthesizer_text_stream() {
        let mut synth = DeltaSynthesizer::new();

        // First text chunk.
        let deltas = synth
            .process_chunk(r#"{"choices":[{"delta":{"content":"Hello"},"finish_reason":null}]}"#)
            .unwrap();
        assert_eq!(deltas.len(), 1);
        assert!(matches!(&deltas[0], StreamDelta::TextDelta { text } if text == "Hello"));

        // Second text chunk.
        let deltas = synth
            .process_chunk(r#"{"choices":[{"delta":{"content":" world"},"finish_reason":null}]}"#)
            .unwrap();
        assert_eq!(deltas.len(), 1);
        assert!(matches!(&deltas[0], StreamDelta::TextDelta { text } if text == " world"));

        // Finish with usage.
        let deltas = synth
            .process_chunk(
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#,
            )
            .unwrap();
        assert!(deltas.iter().any(|d| matches!(
            d,
            StreamDelta::Usage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
                ..
            }
        )));
        assert!(deltas.iter().any(|d| matches!(
            d,
            StreamDelta::Done {
                stop_reason: Some(StopReason::EndTurn),
            }
        )));

        // DONE sentinel — no deltas.
        let deltas = synth.process_chunk("[DONE]").unwrap();
        assert!(deltas.is_empty());
    }

    #[test]
    fn synthesizer_tool_call_stream() {
        let mut synth = DeltaSynthesizer::new();

        // Tool call start.
        let deltas = synth
            .process_chunk(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_abc","function":{"name":"get_weather","arguments":""}}]},"finish_reason":null}]}"#,
            )
            .unwrap();
        assert!(deltas.iter().any(|d| matches!(
            d,
            StreamDelta::ToolCallStart { id, name }
            if id == "call_abc" && name == "get_weather"
        )));

        // Tool call arguments.
        let deltas = synth
            .process_chunk(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"location\":"}}]},"finish_reason":null}]}"#,
            )
            .unwrap();
        assert_eq!(deltas.len(), 1);
        assert!(matches!(
            &deltas[0],
            StreamDelta::ToolCallDelta { id, partial_json }
            if id == "call_abc" && partial_json == r#"{"location":"#
        ));

        // Finish with tool_calls reason.
        let deltas = synth
            .process_chunk(
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":20,"completion_tokens":15}}"#,
            )
            .unwrap();
        assert!(deltas.iter().any(|d| matches!(
            d,
            StreamDelta::Done {
                stop_reason: Some(StopReason::ToolUse),
            }
        )));
    }

    #[test]
    fn synthesizer_rejects_empty_stop_completion() {
        let mut synth = DeltaSynthesizer::new();

        let err = synth
            .process_chunk(
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":0}}"#,
            )
            .unwrap_err();
        assert!(err.contains("empty completion"), "got: {err}");
    }

    #[test]
    fn synthesizer_flushes_late_usage_at_done() {
        let mut synth = DeltaSynthesizer::new();

        // Text in first chunk; finish_reason in second chunk; usage in a
        // third trailing chunk. The usage delta must still be emitted, here
        // at the [DONE] sentinel.
        let _ = synth
            .process_chunk(r#"{"choices":[{"delta":{"content":"hi"},"finish_reason":null}]}"#)
            .unwrap();
        let mid = synth
            .process_chunk(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#)
            .unwrap();
        assert!(mid.iter().any(|d| matches!(
            d,
            StreamDelta::Done {
                stop_reason: Some(StopReason::EndTurn)
            }
        )));
        // Usage hasn't arrived yet, so it must not be in `mid`.
        assert!(!mid.iter().any(|d| matches!(d, StreamDelta::Usage { .. })));

        let _ = synth
            .process_chunk(r#"{"choices":[],"usage":{"prompt_tokens":42,"completion_tokens":7}}"#)
            .unwrap();
        let done = synth.process_chunk("[DONE]").unwrap();
        assert!(done.iter().any(|d| matches!(
            d,
            StreamDelta::Usage {
                input_tokens: 42,
                output_tokens: 7,
                ..
            }
        )));
    }

    #[test]
    fn synthesizer_extracts_openrouter_cost_and_cache_write() {
        let mut synth = DeltaSynthesizer::new();

        // OpenRouter routing an Anthropic-backed model: cache_write_tokens
        // populated, plus top-level cost and reasoning tokens.
        synth
            .process_chunk(r#"{"choices":[{"delta":{"content":"hi"},"finish_reason":null}]}"#)
            .unwrap();
        let deltas = synth
            .process_chunk(
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1500,"completion_tokens":300,"prompt_tokens_details":{"cached_tokens":900,"cache_write_tokens":400},"completion_tokens_details":{"reasoning_tokens":120},"cost":0.00785,"cost_details":{"upstream_inference_cost":0.00712}}}"#,
            )
            .unwrap();

        let usage = deltas
            .iter()
            .find_map(|d| match d {
                StreamDelta::Usage {
                    input_tokens,
                    output_tokens,
                    cache_read_input_tokens,
                    cache_creation_input_tokens,
                    reasoning_output_tokens,
                    cost_usd,
                    upstream_inference_cost_usd,
                } => Some((
                    *input_tokens,
                    *output_tokens,
                    *cache_read_input_tokens,
                    *cache_creation_input_tokens,
                    *reasoning_output_tokens,
                    *cost_usd,
                    *upstream_inference_cost_usd,
                )),
                _ => None,
            })
            .expect("usage delta");
        assert_eq!(usage.0, 1500);
        assert_eq!(usage.1, 300);
        assert_eq!(usage.2, 900);
        assert_eq!(usage.3, 400);
        assert_eq!(usage.4, 120);
        assert_eq!(usage.5, Some(0.00785));
        assert_eq!(usage.6, Some(0.00712));
    }

    #[test]
    fn synthesizer_direct_openai_leaves_cost_none() {
        let mut synth = DeltaSynthesizer::new();
        synth
            .process_chunk(r#"{"choices":[{"delta":{"content":"hi"},"finish_reason":null}]}"#)
            .unwrap();
        let deltas = synth
            .process_chunk(
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":3}}}"#,
            )
            .unwrap();

        let (cache_write, cost, upstream) = deltas
            .iter()
            .find_map(|d| match d {
                StreamDelta::Usage {
                    cache_creation_input_tokens,
                    cost_usd,
                    upstream_inference_cost_usd,
                    ..
                } => Some((
                    *cache_creation_input_tokens,
                    *cost_usd,
                    *upstream_inference_cost_usd,
                )),
                _ => None,
            })
            .expect("usage delta");
        assert_eq!(cache_write, 0, "direct OpenAI never has cache_write");
        assert_eq!(cost, None, "direct OpenAI never has cost");
        assert_eq!(upstream, None);
    }

    #[test]
    fn synthesizer_accepts_stop_after_text() {
        let mut synth = DeltaSynthesizer::new();

        synth
            .process_chunk(r#"{"choices":[{"delta":{"content":"hi"},"finish_reason":null}]}"#)
            .unwrap();

        let deltas = synth
            .process_chunk(
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":1}}"#,
            )
            .unwrap();
        assert!(deltas.iter().any(|d| matches!(
            d,
            StreamDelta::Done {
                stop_reason: Some(StopReason::EndTurn),
            }
        )));
    }

    #[test]
    fn synthesizer_interleaved_tool_calls() {
        let mut synth = DeltaSynthesizer::new();

        // Two tool calls start in the same chunk.
        let deltas = synth
            .process_chunk(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"search","arguments":""}},{"index":1,"id":"call_2","function":{"name":"fetch","arguments":""}}]},"finish_reason":null}]}"#,
            )
            .unwrap();
        assert!(deltas
            .iter()
            .any(|d| matches!(d, StreamDelta::ToolCallStart { id, .. } if id == "call_1")));
        assert!(deltas
            .iter()
            .any(|d| matches!(d, StreamDelta::ToolCallStart { id, .. } if id == "call_2")));

        // Interleaved argument deltas.
        let deltas = synth
            .process_chunk(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"q\":\"rust\"}"}}]},"finish_reason":null}]}"#,
            )
            .unwrap();
        assert!(matches!(&deltas[0], StreamDelta::ToolCallDelta { id, .. } if id == "call_1"));

        let deltas = synth
            .process_chunk(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{\"url\":\"https://example.com\"}"}}]},"finish_reason":null}]}"#,
            )
            .unwrap();
        assert!(matches!(&deltas[0], StreamDelta::ToolCallDelta { id, .. } if id == "call_2"));
    }

    fn anthropic_via_openrouter_prompt() -> llm::Prompt {
        llm::Prompt {
            chain: llm::ModelChain::new("anthropic/claude-sonnet-4.6"),
            effort: llm::ReasoningEffort::Low,
            system: vec![SystemBlock::Text {
                text: "system instructions".to_string(),
            }],
            messages: vec![Message::User {
                content: vec![UserBlock::Text {
                    text: "user question".to_string(),
                }],
            }],
            tools: vec![Tool {
                name: "fetch".to_string(),
                description: None,
                input_schema: serde_json::json!({"type": "object"}),
                strict: false,
            }],
            tool_choice: None,
            max_tokens: Some(1024),
            cache_key: None,
        }
    }

    #[test]
    fn anthropic_via_openrouter_marks_last_user_text_block() {
        let req = prompt_to_request(
            &anthropic_via_openrouter_prompt(),
            ReasoningDialect::OpenRouter,
        );

        let last = req.messages.last().expect("messages present");
        assert_eq!(last.role, "user");
        let blocks = match last.content.as_ref().expect("content present") {
            OpenAiMessageContent::Blocks(b) => b,
            OpenAiMessageContent::Text(_) => {
                panic!("expected array-of-blocks shape for cache_control")
            }
        };
        let marker = blocks.last().and_then(cache_marker).expect("cache marker");
        assert_eq!(marker.r#type, "ephemeral");
        assert_eq!(marker.ttl, None, "default 5m TTL — omit field");

        // Earlier non-marked blocks (system) keep the plain string shape so we
        // don't churn requests for non-Anthropic-targeted system text.
        let system = req.messages.first().expect("system message");
        assert!(matches!(
            system.content,
            Some(OpenAiMessageContent::Text(_))
        ));
        assert!(req
            .tools
            .as_ref()
            .and_then(|v| v.last())
            .and_then(|t| t.cache_control.as_ref())
            .is_none());
    }

    #[test]
    fn non_anthropic_model_emits_plain_string_content() {
        let mut prompt = anthropic_via_openrouter_prompt();
        prompt.chain = llm::ModelChain::new("openai/gpt-5-mini");

        let req = prompt_to_request(&prompt, ReasoningDialect::OpenRouter);

        for msg in &req.messages {
            assert!(
                matches!(msg.content, Some(OpenAiMessageContent::Text(_)) | None),
                "non-Anthropic models must keep plain-string content"
            );
        }
        assert!(req
            .tools
            .as_ref()
            .and_then(|v| v.last())
            .and_then(|t| t.cache_control.as_ref())
            .is_none());

        let json = serde_json::to_value(&req).unwrap();
        assert!(
            !json.to_string().contains("cache_control"),
            "wire JSON must not contain cache_control for non-Anthropic models"
        );
    }

    #[test]
    fn anthropic_falls_through_to_tool_when_messages_empty() {
        let mut prompt = anthropic_via_openrouter_prompt();
        prompt.messages.clear();
        prompt.system.clear();

        let req = prompt_to_request(&prompt, ReasoningDialect::OpenRouter);

        let tool = req
            .tools
            .as_ref()
            .and_then(|v| v.last())
            .expect("tool present");
        let marker = tool.cache_control.as_ref().expect("tool cache marker");
        assert_eq!(marker.r#type, "ephemeral");
    }

    #[test]
    fn anthropic_serialised_request_has_cache_control_inside_content_block() {
        let req = prompt_to_request(
            &anthropic_via_openrouter_prompt(),
            ReasoningDialect::OpenRouter,
        );
        let json = serde_json::to_value(&req).unwrap();

        // Wire-format check: the marker is nested inside a content block on
        // the user message. This is what OpenRouter forwards to Anthropic.
        let user_msg = json["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "user")
            .unwrap();
        let last_block = user_msg["content"].as_array().unwrap().last().unwrap();
        assert_eq!(last_block["type"], "text");
        assert_eq!(last_block["cache_control"]["type"], "ephemeral");
        assert!(
            last_block
                .get("cache_control")
                .unwrap()
                .get("ttl")
                .is_none(),
            "default TTL should be omitted from wire JSON"
        );
    }
}
