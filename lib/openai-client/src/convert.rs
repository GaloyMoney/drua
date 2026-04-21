//! Conversion functions between the provider-agnostic `llm` types and the
//! OpenAI-specific internal types.
//!
//! These adapters sit at the boundary of `OpenAiClient`: inbound `Prompt`
//! values are converted to `OpenAiRequest` for the wire, and streaming
//! response chunks are converted to `StreamDelta`.

use llm::prompt::{
    AssistantBlock, Message, SystemBlock, Tool, ToolChoice, ToolResultBlock, UserBlock,
};
use llm::stream::{ContentBlockType, StreamDelta};
use llm::StopReason;

use crate::types::{
    OpenAiFunction, OpenAiMessage, OpenAiRequest, OpenAiRequestToolCall, OpenAiStreamChunk,
    OpenAiTool, OpenAiToolChoice, OpenAiToolChoiceFunction, OpenAiToolFunction, StreamOptions,
};

// ============================================================================
// Prompt → OpenAiRequest
// ============================================================================

/// Convert a provider-agnostic `Prompt` into an OpenAI-specific request
/// body with `stream: true`.
pub(crate) fn prompt_to_request(prompt: &llm::Prompt) -> OpenAiRequest {
    let mut messages: Vec<OpenAiMessage> = Vec::new();

    // System blocks become system messages (one per block).
    for block in &prompt.system {
        match block {
            SystemBlock::Text { text, .. } => {
                messages.push(OpenAiMessage {
                    role: "system",
                    content: Some(text.clone()),
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
        }
    }

    // Conversation messages.
    for msg in &prompt.messages {
        convert_message(msg, &mut messages);
    }

    let tools = if prompt.tools.is_empty() {
        None
    } else {
        Some(prompt.tools.iter().map(convert_tool).collect())
    };

    let tool_choice = prompt.tool_choice.as_ref().map(convert_tool_choice);

    OpenAiRequest {
        model: prompt.model.clone(),
        messages,
        max_completion_tokens: prompt.max_tokens,
        temperature: None,
        tools,
        tool_choice,
        stream: true,
        stream_options: Some(StreamOptions {
            include_usage: true,
        }),
    }
}

fn convert_message(message: &Message, out: &mut Vec<OpenAiMessage>) {
    match message {
        Message::User { content } => {
            // Merge consecutive text blocks into a single user message.
            // Tool results become separate "tool" role messages.
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
                        // Flush any pending text first.
                        if !text_parts.is_empty() {
                            out.push(OpenAiMessage {
                                role: "user",
                                content: Some(text_parts.join("\n")),
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
                            content: Some(text),
                            tool_calls: None,
                            tool_call_id: Some(tool_use_id.clone()),
                        });
                    }
                }
            }
            if !text_parts.is_empty() {
                out.push(OpenAiMessage {
                    role: "user",
                    content: Some(text_parts.join("\n")),
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
        }
        Message::Assistant { content } => {
            // Group text and tool calls into a single assistant message.
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
                Some(text_parts.join("\n"))
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
        },
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

// ============================================================================
// Streaming chunk → StreamDelta(s)
// ============================================================================

/// State tracker for synthesizing ContentBlockStart/Stop events that the
/// StreamAccumulator expects but OpenAI doesn't natively emit.
pub(crate) struct DeltaSynthesizer {
    /// Whether we've emitted a ContentBlockStart for the text block.
    text_block_started: bool,
    /// Maps tool_call index to the content block index we assigned.
    /// Tracks which tool call indices we've already emitted ContentBlockStart for.
    tool_block_indices: Vec<Option<usize>>,
    /// Next content block index to assign.
    next_block_index: usize,
    /// Total open blocks (for emitting ContentBlockStop on finish).
    open_blocks: usize,
    /// Input tokens (captured from usage chunk).
    input_tokens: u32,
}

impl DeltaSynthesizer {
    pub fn new() -> Self {
        Self {
            text_block_started: false,
            tool_block_indices: Vec::new(),
            next_block_index: 0,
            open_blocks: 0,
            input_tokens: 0,
        }
    }

    /// Process a raw SSE data payload (OpenAI JSON) and emit provider-agnostic
    /// `StreamDelta`s. Returns `Ok(None)` for the `[DONE]` sentinel.
    pub fn process_chunk(&mut self, data: &str) -> Result<Vec<StreamDelta>, String> {
        if data.trim() == "[DONE]" {
            return Ok(vec![StreamDelta::MessageStop]);
        }

        let chunk: OpenAiStreamChunk =
            serde_json::from_str(data).map_err(|e| format!("JSON parse: {e}"))?;

        let mut deltas = Vec::new();

        // Capture usage if present (OpenAI sends it in a separate chunk when
        // stream_options.include_usage is true).
        if let Some(usage) = &chunk.usage {
            self.input_tokens = usage.prompt_tokens;
            // Emit MessageStart with input tokens.
            deltas.push(StreamDelta::MessageStart {
                input_tokens: usage.prompt_tokens,
            });
        }

        let Some(choice) = chunk.choices.first() else {
            return Ok(deltas);
        };

        if let Some(delta) = &choice.delta {
            // Handle text content.
            if let Some(text) = &delta.content {
                if !text.is_empty() {
                    if !self.text_block_started {
                        self.text_block_started = true;
                        let idx = self.next_block_index;
                        self.next_block_index += 1;
                        self.open_blocks += 1;
                        deltas.push(StreamDelta::ContentBlockStart {
                            index: idx,
                            block_type: ContentBlockType::Text,
                        });
                    }
                    deltas.push(StreamDelta::TextDelta {
                        index: 0,
                        text: text.clone(),
                    });
                }
            }

            // Handle tool calls.
            if let Some(tool_calls) = &delta.tool_calls {
                for tc in tool_calls {
                    // Ensure our tracking vec is large enough.
                    while self.tool_block_indices.len() <= tc.index {
                        self.tool_block_indices.push(None);
                    }

                    // First chunk for this tool call — emit ContentBlockStart.
                    if self.tool_block_indices[tc.index].is_none() {
                        let block_idx = self.next_block_index;
                        self.next_block_index += 1;
                        self.open_blocks += 1;
                        self.tool_block_indices[tc.index] = Some(block_idx);

                        let id = tc.id.clone().unwrap_or_default();
                        let name = tc
                            .function
                            .as_ref()
                            .and_then(|f| f.name.clone())
                            .unwrap_or_default();

                        deltas.push(StreamDelta::ContentBlockStart {
                            index: block_idx,
                            block_type: ContentBlockType::ToolUse { id, name },
                        });
                    }

                    // Argument deltas.
                    if let Some(func) = &tc.function {
                        if let Some(args) = &func.arguments {
                            if !args.is_empty() {
                                let block_idx = self.tool_block_indices[tc.index].unwrap();
                                deltas.push(StreamDelta::InputJsonDelta {
                                    index: block_idx,
                                    partial_json: args.clone(),
                                });
                            }
                        }
                    }
                }
            }
        }

        // Handle finish_reason.
        if let Some(reason) = &choice.finish_reason {
            // Close all open blocks.
            for idx in 0..self.next_block_index {
                deltas.push(StreamDelta::ContentBlockStop { index: idx });
            }

            let stop_reason = match reason.as_str() {
                "stop" => Some(StopReason::EndTurn),
                "length" => Some(StopReason::MaxTokens),
                "tool_calls" => Some(StopReason::ToolUse),
                _ => None,
            };

            // Output tokens come from the usage chunk. If we haven't seen it
            // yet (it may arrive after this chunk), use 0 — it will be
            // populated when the usage chunk arrives.
            let output_tokens = chunk
                .usage
                .as_ref()
                .map(|u| u.completion_tokens)
                .unwrap_or(0);

            deltas.push(StreamDelta::MessageDelta {
                stop_reason,
                output_tokens,
            });
        }

        Ok(deltas)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_prompt() -> llm::Prompt {
        llm::Prompt {
            model: "gpt-4o".to_string(),
            system: vec![SystemBlock::Text {
                text: "You are a helpful assistant.".to_string(),
                cache_control: None,
            }],
            messages: vec![Message::User {
                content: vec![UserBlock::Text {
                    text: "Hello".to_string(),
                    cache_control: None,
                }],
            }],
            tools: vec![Tool {
                name: "get_weather".to_string(),
                description: Some("Get weather".to_string()),
                input_schema: serde_json::json!({"type": "object", "properties": {"location": {"type": "string"}}}),
                cache_control: None,
            }],
            tool_choice: None,
            max_tokens: Some(1024),
        }
    }

    #[test]
    fn prompt_to_request_basic() {
        let prompt = sample_prompt();
        let req = prompt_to_request(&prompt);

        assert_eq!(req.model, "gpt-4o");
        assert!(req.stream);
        assert_eq!(req.messages.len(), 2); // system + user
        assert_eq!(req.messages[0].role, "system");
        assert_eq!(req.messages[1].role, "user");
        assert_eq!(req.max_completion_tokens, Some(1024));
        assert!(req.tools.is_some());
        assert_eq!(req.tools.as_ref().unwrap().len(), 1);
        assert_eq!(req.tools.as_ref().unwrap()[0].function.name, "get_weather");
    }

    #[test]
    fn prompt_with_tool_results() {
        let prompt = llm::Prompt {
            model: "gpt-4o".to_string(),
            system: vec![],
            messages: vec![
                Message::Assistant {
                    content: vec![AssistantBlock::ToolUse {
                        id: "call_123".to_string(),
                        name: "get_weather".to_string(),
                        input: serde_json::json!({"location": "NYC"}),
                        cache_control: None,
                    }],
                },
                Message::User {
                    content: vec![UserBlock::ToolResult {
                        tool_use_id: "call_123".to_string(),
                        content: vec![ToolResultBlock::Text {
                            text: "72F".to_string(),
                        }],
                        is_error: false,
                        cache_control: None,
                    }],
                },
            ],
            tools: vec![],
            tool_choice: None,
            max_tokens: None,
        };

        let req = prompt_to_request(&prompt);
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[0].role, "assistant");
        assert!(req.messages[0].tool_calls.is_some());
        assert_eq!(req.messages[1].role, "tool");
        assert_eq!(req.messages[1].tool_call_id.as_deref(), Some("call_123"));
        assert_eq!(req.messages[1].content.as_deref(), Some("72F"));
    }

    #[test]
    fn thinking_blocks_dropped() {
        let prompt = llm::Prompt {
            model: "gpt-4o".to_string(),
            system: vec![],
            messages: vec![Message::Assistant {
                content: vec![
                    AssistantBlock::Thinking {
                        text: "internal reasoning".to_string(),
                        signature: Some("sig".to_string()),
                    },
                    AssistantBlock::Text {
                        text: "visible response".to_string(),
                        cache_control: None,
                    },
                ],
            }],
            tools: vec![],
            tool_choice: None,
            max_tokens: None,
        };

        let req = prompt_to_request(&prompt);
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].content.as_deref(), Some("visible response"));
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

        // First text chunk — should synthesize ContentBlockStart.
        let deltas = synth
            .process_chunk(r#"{"choices":[{"delta":{"content":"Hello"},"finish_reason":null}]}"#)
            .unwrap();
        assert_eq!(deltas.len(), 2); // ContentBlockStart + TextDelta
        assert!(matches!(
            &deltas[0],
            StreamDelta::ContentBlockStart {
                index: 0,
                block_type: ContentBlockType::Text
            }
        ));
        assert!(matches!(&deltas[1], StreamDelta::TextDelta { index: 0, text } if text == "Hello"));

        // Second text chunk — no ContentBlockStart.
        let deltas = synth
            .process_chunk(r#"{"choices":[{"delta":{"content":" world"},"finish_reason":null}]}"#)
            .unwrap();
        assert_eq!(deltas.len(), 1);
        assert!(
            matches!(&deltas[0], StreamDelta::TextDelta { index: 0, text } if text == " world")
        );

        // Finish.
        let deltas = synth
            .process_chunk(
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#,
            )
            .unwrap();
        // MessageStart (from usage) + ContentBlockStop + MessageDelta
        assert!(deltas
            .iter()
            .any(|d| matches!(d, StreamDelta::MessageStart { input_tokens: 10 })));
        assert!(deltas
            .iter()
            .any(|d| matches!(d, StreamDelta::ContentBlockStop { index: 0 })));
        assert!(deltas.iter().any(|d| matches!(
            d,
            StreamDelta::MessageDelta {
                stop_reason: Some(StopReason::EndTurn),
                ..
            }
        )));

        // DONE sentinel.
        let deltas = synth.process_chunk("[DONE]").unwrap();
        assert_eq!(deltas.len(), 1);
        assert!(matches!(deltas[0], StreamDelta::MessageStop));
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
            StreamDelta::ContentBlockStart {
                index: 0,
                block_type: ContentBlockType::ToolUse { .. }
            }
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
            StreamDelta::InputJsonDelta {
                index: 0,
                partial_json
            } if partial_json == r#"{"location":"#
        ));

        // Finish with tool_calls reason.
        let deltas = synth
            .process_chunk(
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":20,"completion_tokens":15}}"#,
            )
            .unwrap();
        assert!(deltas.iter().any(|d| matches!(
            d,
            StreamDelta::MessageDelta {
                stop_reason: Some(StopReason::ToolUse),
                ..
            }
        )));
    }
}
