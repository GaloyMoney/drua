//! Conversion functions between the provider-agnostic `llm` types and the
//! Anthropic-specific internal types.
//!
//! These adapters sit at the boundary of `AnthropicClient`: inbound `Prompt`
//! values are converted to `AnthropicRequest` for the wire, and the
//! accumulated streaming result is converted back to `PromptResponse`.

use llm::prompt::{
    AssistantBlock, CacheControl, CacheTtl, Message, SystemBlock, Tool, ToolChoice,
    ToolResultBlock, UserBlock,
};
use llm::{PromptResponse, StopReason, Usage};

use llm::stream::StreamDelta;

use crate::stream::{AccumulatedBlock, AccumulatedResponse, AccumulatedStopReason};
use crate::types::{
    AnthropicCacheControl, AnthropicContent, AnthropicContentBlock, AnthropicDelta,
    AnthropicMessage, AnthropicRequest, AnthropicStopReason, AnthropicStreamEvent,
    AnthropicSystemBlock, AnthropicTool, AnthropicToolChoice, AnthropicToolResultContent,
};

// ============================================================================
// Prompt → AnthropicRequest
// ============================================================================

/// Convert a provider-agnostic `Prompt` into an Anthropic-specific request
/// body with `stream: true`.
pub(crate) fn prompt_to_request(prompt: &llm::Prompt) -> AnthropicRequest {
    let messages: Vec<AnthropicMessage> = prompt.messages.iter().map(convert_message).collect();

    let system = if prompt.system.is_empty() {
        None
    } else {
        Some(prompt.system.iter().map(convert_system_block).collect())
    };

    let tools = if prompt.tools.is_empty() {
        None
    } else {
        Some(prompt.tools.iter().map(convert_tool).collect())
    };

    let tool_choice = prompt.tool_choice.as_ref().map(convert_tool_choice);

    AnthropicRequest {
        model: prompt.model.clone(),
        messages,
        system,
        max_tokens: prompt.max_tokens.unwrap_or(8192),
        temperature: None,
        tools,
        tool_choice,
        stream: true,
        thinking: None,
    }
}

fn convert_system_block(block: &SystemBlock) -> AnthropicSystemBlock {
    match block {
        SystemBlock::Text {
            text,
            cache_control,
        } => AnthropicSystemBlock {
            r#type: "text".to_string(),
            text: text.clone(),
            cache_control: cache_control.as_ref().map(convert_cache_control),
        },
    }
}

fn convert_message(message: &Message) -> AnthropicMessage {
    match message {
        Message::User { content } => AnthropicMessage {
            role: "user",
            content: content.iter().map(convert_user_block).collect(),
        },
        Message::Assistant { content } => AnthropicMessage {
            role: "assistant",
            content: content.iter().map(convert_assistant_block).collect(),
        },
    }
}

fn convert_user_block(block: &UserBlock) -> AnthropicContent {
    match block {
        UserBlock::Text {
            text,
            cache_control,
        } => AnthropicContent::Text {
            text: text.clone(),
            cache_control: cache_control.as_ref().map(convert_cache_control),
        },
        UserBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
            cache_control,
        } => AnthropicContent::ToolResult {
            tool_use_id: tool_use_id.clone(),
            content: content.iter().map(convert_tool_result_block).collect(),
            is_error: if *is_error { Some(true) } else { None },
            cache_control: cache_control.as_ref().map(convert_cache_control),
        },
    }
}

fn convert_tool_result_block(block: &ToolResultBlock) -> AnthropicToolResultContent {
    match block {
        ToolResultBlock::Text { text } => AnthropicToolResultContent::Text { text: text.clone() },
    }
}

fn convert_assistant_block(block: &AssistantBlock) -> AnthropicContent {
    match block {
        AssistantBlock::Text {
            text,
            cache_control,
        } => AnthropicContent::Text {
            text: text.clone(),
            cache_control: cache_control.as_ref().map(convert_cache_control),
        },
        AssistantBlock::ToolUse {
            id,
            name,
            input,
            cache_control,
        } => AnthropicContent::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
            cache_control: cache_control.as_ref().map(convert_cache_control),
        },
        AssistantBlock::Thinking { text, signature } => AnthropicContent::Thinking {
            thinking: text.clone(),
            signature: signature.clone().unwrap_or_default(),
        },
    }
}

fn convert_tool(tool: &Tool) -> AnthropicTool {
    AnthropicTool {
        name: tool.name.clone(),
        description: tool.description.clone(),
        input_schema: tool.input_schema.clone(),
        cache_control: tool.cache_control.as_ref().map(convert_cache_control),
    }
}

fn convert_tool_choice(choice: &ToolChoice) -> AnthropicToolChoice {
    match choice {
        ToolChoice::Auto => AnthropicToolChoice::Auto,
        ToolChoice::Any => AnthropicToolChoice::Any,
        ToolChoice::None => AnthropicToolChoice::None,
        ToolChoice::Tool { name } => AnthropicToolChoice::Tool { name: name.clone() },
    }
}

fn convert_cache_control(cc: &CacheControl) -> AnthropicCacheControl {
    match cc {
        CacheControl::Ephemeral { ttl } => AnthropicCacheControl {
            r#type: "ephemeral".to_string(),
            ttl: ttl.as_ref().map(|t| match t {
                CacheTtl::FiveMinutes => "5m".to_string(),
                CacheTtl::OneHour => "1h".to_string(),
            }),
        },
    }
}

// ============================================================================
// AccumulatedResponse → PromptResponse
// ============================================================================

/// Convert the internally-accumulated streaming result back to the
/// provider-agnostic `PromptResponse` that callers expect.
pub(crate) fn accumulated_to_response(acc: AccumulatedResponse) -> PromptResponse {
    let content = acc
        .content
        .into_iter()
        .map(convert_accumulated_block)
        .collect();

    let stop_reason = acc.stop_reason.map(|sr| match sr {
        AccumulatedStopReason::EndTurn => StopReason::EndTurn,
        AccumulatedStopReason::MaxTokens => StopReason::MaxTokens,
        AccumulatedStopReason::ToolUse => StopReason::ToolUse,
        AccumulatedStopReason::StopSequence => StopReason::StopSequence,
    });

    // Truncate u64 → u32 (safe for realistic token counts).
    let usage = Usage {
        input_tokens: acc.usage.input_tokens as u32,
        output_tokens: acc.usage.output_tokens as u32,
    };

    PromptResponse {
        content,
        usage,
        stop_reason,
    }
}

fn convert_accumulated_block(block: AccumulatedBlock) -> AssistantBlock {
    match block {
        AccumulatedBlock::Text { text } => AssistantBlock::Text {
            text,
            cache_control: None,
        },
        AccumulatedBlock::Thinking {
            thinking,
            signature,
        } => AssistantBlock::Thinking {
            text: thinking,
            signature,
        },
        AccumulatedBlock::ToolUse {
            id,
            name,
            arguments,
        } => AssistantBlock::ToolUse {
            id,
            name,
            input: arguments,
            cache_control: None,
        },
    }
}

// ============================================================================
// SSE JSON → StreamDelta (stateful converter)
// ============================================================================

/// Tracks Anthropic content-block indices so that index-only deltas
/// (e.g. `InputJsonDelta`) can be mapped to the tool call ID that was
/// announced in the preceding `ContentBlockStart`.
pub(crate) struct AnthropicDeltaConverter {
    blocks: Vec<BlockInfo>,
}

enum BlockInfo {
    Text,
    Thinking,
    ToolUse { id: String },
}

impl AnthropicDeltaConverter {
    pub fn new() -> Self {
        Self { blocks: Vec::new() }
    }

    /// Process a raw SSE data payload and return zero or more provider-agnostic
    /// [`StreamDelta`]s. Returns an empty vec for events with no meaningful
    /// delta (e.g. `Ping`, `ContentBlockStop`, `MessageStop`).
    pub fn process_event(&mut self, data: &str) -> Result<Vec<StreamDelta>, String> {
        let event: AnthropicStreamEvent =
            serde_json::from_str(data).map_err(|e| format!("JSON parse: {e}"))?;

        Ok(match event {
            AnthropicStreamEvent::MessageStart { message } => {
                let input_tokens = message.usage.map(|u| u.input as u32).unwrap_or(0);
                vec![StreamDelta::Usage {
                    input_tokens,
                    output_tokens: 0,
                }]
            }
            AnthropicStreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                let idx = index as usize;
                // Grow the blocks vec to accommodate this index.
                while self.blocks.len() <= idx {
                    self.blocks.push(BlockInfo::Text);
                }
                match content_block {
                    AnthropicContentBlock::Text => {
                        self.blocks[idx] = BlockInfo::Text;
                        vec![]
                    }
                    AnthropicContentBlock::Thinking => {
                        self.blocks[idx] = BlockInfo::Thinking;
                        vec![]
                    }
                    AnthropicContentBlock::ToolUse { id, name } => {
                        let id = id.unwrap_or_default();
                        let name = name.unwrap_or_default();
                        self.blocks[idx] = BlockInfo::ToolUse { id: id.clone() };
                        vec![StreamDelta::ToolCallStart { id, name }]
                    }
                }
            }
            AnthropicStreamEvent::ContentBlockDelta { index, delta } => {
                let idx = index as usize;
                match delta {
                    AnthropicDelta::TextDelta { text } => text
                        .map(|t| vec![StreamDelta::TextDelta { text: t }])
                        .unwrap_or_default(),
                    AnthropicDelta::ThinkingDelta { thinking } => thinking
                        .map(|t| vec![StreamDelta::ThinkingDelta { text: t }])
                        .unwrap_or_default(),
                    AnthropicDelta::InputJsonDelta { partial_json } => {
                        partial_json
                            .map(|j| {
                                let id = match self.blocks.get(idx) {
                                    Some(BlockInfo::ToolUse { id }) => id.clone(),
                                    _ => String::new(),
                                };
                                vec![StreamDelta::ToolCallDelta {
                                    id,
                                    partial_json: j,
                                }]
                            })
                            .unwrap_or_default()
                    }
                    AnthropicDelta::SignatureDelta { signature } => signature
                        .map(|s| vec![StreamDelta::ThinkingSignature { signature: s }])
                        .unwrap_or_default(),
                }
            }
            AnthropicStreamEvent::ContentBlockStop { .. } => vec![],
            AnthropicStreamEvent::MessageDelta { delta, usage } => {
                let mut deltas = Vec::new();
                if let Some(u) = usage {
                    deltas.push(StreamDelta::Usage {
                        input_tokens: 0,
                        output_tokens: u.output_tokens as u32,
                    });
                }
                let stop_reason = delta.stop_reason.map(|sr| match sr {
                    AnthropicStopReason::EndTurn => StopReason::EndTurn,
                    AnthropicStopReason::MaxTokens => StopReason::MaxTokens,
                    AnthropicStopReason::ToolUse => StopReason::ToolUse,
                    AnthropicStopReason::StopSequence => StopReason::StopSequence,
                });
                deltas.push(StreamDelta::Done { stop_reason });
                deltas
            }
            AnthropicStreamEvent::MessageStop => vec![],
            AnthropicStreamEvent::Error { error } => {
                vec![StreamDelta::Error {
                    message: error.message,
                }]
            }
            AnthropicStreamEvent::Ping => vec![],
        })
    }
}
