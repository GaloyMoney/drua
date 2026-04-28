//! Adapters between the provider-agnostic `llm` types and OpenAI wire types.

use llm::prompt::{
    AssistantBlock, Message, SystemBlock, Tool, ToolChoice, ToolResultBlock, UserBlock,
};
use llm::stream::StreamDelta;
use llm::StopReason;

use crate::types::{
    OpenAiFunction, OpenAiMessage, OpenAiRequest, OpenAiRequestToolCall, OpenAiStreamChunk,
    OpenAiTool, OpenAiToolChoice, OpenAiToolChoiceFunction, OpenAiToolFunction, StreamOptions,
};

pub(crate) fn prompt_to_request(prompt: &llm::Prompt) -> OpenAiRequest {
    let mut messages: Vec<OpenAiMessage> = Vec::new();

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
        prompt_cache_key: prompt.cache_key.clone(),
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

/// Converts OpenAI streaming chunks into provider-agnostic [`StreamDelta`]s.
/// Tracks tool call IDs by index so subsequent argument deltas can reference them.
pub(crate) struct DeltaSynthesizer {
    tool_ids: Vec<Option<String>>,
}

impl DeltaSynthesizer {
    pub fn new() -> Self {
        Self {
            tool_ids: Vec::new(),
        }
    }

    pub fn process_chunk(&mut self, data: &str) -> Result<Vec<StreamDelta>, String> {
        if data.trim() == "[DONE]" {
            return Ok(vec![]);
        }

        let chunk: OpenAiStreamChunk =
            serde_json::from_str(data).map_err(|e| format!("JSON parse: {e}"))?;

        let mut deltas = Vec::new();

        // OpenAI emits usage in a separate chunk when stream_options.include_usage is set.
        if let Some(usage) = &chunk.usage {
            let cached_tokens = usage
                .prompt_tokens_details
                .as_ref()
                .map(|details| details.cached_tokens)
                .unwrap_or(0);
            deltas.push(StreamDelta::Usage {
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
                cache_read_input_tokens: cached_tokens,
                cache_creation_input_tokens: 0,
            });
        }

        let Some(choice) = chunk.choices.first() else {
            return Ok(deltas);
        };

        if let Some(delta) = &choice.delta {
            if let Some(text) = &delta.content {
                if !text.is_empty() {
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
            cache_key: None,
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
        assert!(req.prompt_cache_key.is_none());
        assert!(req.tools.is_some());
        assert_eq!(req.tools.as_ref().unwrap().len(), 1);
        assert_eq!(req.tools.as_ref().unwrap()[0].function.name, "get_weather");
    }

    #[test]
    fn prompt_to_request_includes_prompt_cache_key() {
        let mut prompt = sample_prompt();
        prompt.cache_key = Some("agent-session:test".to_string());

        let req = prompt_to_request(&prompt);
        assert_eq!(req.prompt_cache_key.as_deref(), Some("agent-session:test"));
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
            cache_key: None,
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
            cache_key: None,
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
}
