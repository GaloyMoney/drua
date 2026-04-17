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

use crate::stream::{AccumulatedBlock, AccumulatedResponse, AccumulatedStopReason};
use crate::types::{
    AnthropicCacheControl, AnthropicContent, AnthropicMessage, AnthropicRequest,
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
