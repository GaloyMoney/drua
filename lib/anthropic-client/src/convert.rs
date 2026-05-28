//! Adapters between `lib/llm` types and Anthropic wire types. Owns the
//! prompt-cache-marker placement strategy — `lib/llm` deliberately
//! knows nothing about Anthropic's prompt caching.

use llm::prompt::{
    AssistantBlock, Message, SystemBlock, Tool, ToolChoice, ToolResultBlock, UserBlock,
};
use llm::{PromptResponse, StopReason, Usage};

use llm::stream::StreamDelta;

use crate::stream::{AccumulatedBlock, AccumulatedResponse, AccumulatedStopReason};
use crate::types::{
    AnthropicCacheControl, AnthropicContent, AnthropicContentBlock, AnthropicDelta,
    AnthropicMessage, AnthropicRequest, AnthropicStopReason, AnthropicStreamEvent,
    AnthropicSystemBlock, AnthropicTool, AnthropicToolChoice, AnthropicToolResultContent,
};

/// 5m matches Anthropic's default cache window.
const DEFAULT_CACHE_TTL: AnthropicCacheTtl = AnthropicCacheTtl::FiveMinutes;

#[derive(Debug, Clone, Copy)]
enum AnthropicCacheTtl {
    FiveMinutes,
    #[allow(dead_code)]
    OneHour,
}

impl AnthropicCacheTtl {
    fn wire(self) -> &'static str {
        match self {
            Self::FiveMinutes => "5m",
            Self::OneHour => "1h",
        }
    }
}

pub(crate) fn prompt_to_request(prompt: &llm::Prompt, spec: &llm::ModelSpec) -> AnthropicRequest {
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

    let mut request = AnthropicRequest {
        model: spec.name.clone(),
        messages,
        system,
        max_tokens: spec.max_tokens.unwrap_or(8192),
        temperature: None,
        tools,
        tool_choice,
        stream: true,
        thinking: None,
    };

    apply_prompt_caching(&mut request, DEFAULT_CACHE_TTL);
    request
}

/// Anthropic auto-checks earlier breakpoints, so one marker on the
/// final cacheable block (last message → last system block → last tool
/// definition, in that order) is sufficient.
fn apply_prompt_caching(req: &mut AnthropicRequest, ttl: AnthropicCacheTtl) {
    let marker = AnthropicCacheControl {
        r#type: "ephemeral".to_string(),
        ttl: Some(ttl.wire().to_string()),
    };

    if let Some(last_msg) = req.messages.last_mut() {
        if mark_message(last_msg, &marker) {
            return;
        }
    }
    if let Some(sys) = req.system.as_mut().and_then(|v| v.last_mut()) {
        sys.cache_control = Some(marker);
        return;
    }
    if let Some(tool) = req.tools.as_mut().and_then(|v| v.last_mut()) {
        tool.cache_control = Some(marker);
    }
}

/// Thinking blocks aren't cacheable on their own — breakpoint must
/// land on text/tool content.
fn mark_message(msg: &mut AnthropicMessage, marker: &AnthropicCacheControl) -> bool {
    for block in msg.content.iter_mut().rev() {
        match block {
            AnthropicContent::Text { cache_control, .. }
            | AnthropicContent::ToolUse { cache_control, .. }
            | AnthropicContent::ToolResult { cache_control, .. } => {
                *cache_control = Some(marker.clone());
                return true;
            }
            AnthropicContent::Thinking { .. } | AnthropicContent::Image { .. } => {}
        }
    }
    false
}

fn convert_system_block(block: &SystemBlock) -> AnthropicSystemBlock {
    match block {
        SystemBlock::Text { text } => AnthropicSystemBlock {
            r#type: "text".to_string(),
            text: text.clone(),
            cache_control: None,
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
        UserBlock::Text { text } => AnthropicContent::Text {
            text: text.clone(),
            cache_control: None,
        },
        UserBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => AnthropicContent::ToolResult {
            tool_use_id: tool_use_id.clone(),
            content: content.iter().map(convert_tool_result_block).collect(),
            is_error: if *is_error { Some(true) } else { None },
            cache_control: None,
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
        AssistantBlock::Text { text } => AnthropicContent::Text {
            text: text.clone(),
            cache_control: None,
        },
        AssistantBlock::ToolUse { id, name, input } => AnthropicContent::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
            cache_control: None,
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
        strict: if tool.strict { Some(true) } else { None },
        cache_control: None,
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

    // u64 → u32 truncation is safe for realistic token counts.
    let usage = Usage {
        input_tokens: (acc.usage.input_tokens
            + acc.usage.cache_read_tokens
            + acc.usage.cache_write_tokens) as u32,
        output_tokens: acc.usage.output_tokens as u32,
        cache_read_input_tokens: acc.usage.cache_read_tokens as u32,
        cache_creation_input_tokens: acc.usage.cache_write_tokens as u32,
        ..Default::default()
    };

    PromptResponse {
        content,
        usage,
        stop_reason,
        model_used: None,
    }
}

fn convert_accumulated_block(block: AccumulatedBlock) -> AssistantBlock {
    match block {
        AccumulatedBlock::Text { text } => AssistantBlock::Text { text },
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
        },
    }
}

/// Tracks content-block indices so index-only deltas (e.g. `InputJsonDelta`)
/// can be mapped to the tool call ID announced in the preceding
/// `ContentBlockStart`.
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

    /// Returns an empty vec for `Ping`/`ContentBlockStop`/`MessageStop`.
    pub fn process_event(&mut self, data: &str) -> Result<Vec<StreamDelta>, String> {
        let event: AnthropicStreamEvent =
            serde_json::from_str(data).map_err(|e| format!("JSON parse: {e}"))?;

        Ok(match event {
            AnthropicStreamEvent::MessageStart { message } => {
                let input_tokens = message
                    .usage
                    .as_ref()
                    .map(|u| {
                        (u.input + u.cache_read.unwrap_or(0) + u.cache_write.unwrap_or(0)) as u32
                    })
                    .unwrap_or(0);
                let cache_read_input_tokens = message
                    .usage
                    .as_ref()
                    .and_then(|u| u.cache_read)
                    .unwrap_or(0) as u32;
                let cache_creation_input_tokens = message
                    .usage
                    .as_ref()
                    .and_then(|u| u.cache_write)
                    .unwrap_or(0) as u32;
                vec![StreamDelta::Usage {
                    input_tokens,
                    output_tokens: 0,
                    cache_read_input_tokens,
                    cache_creation_input_tokens,
                    reasoning_output_tokens: 0,
                    cost_usd: None,
                    upstream_inference_cost_usd: None,
                }]
            }
            AnthropicStreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                let idx = index as usize;
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
                    AnthropicDelta::InputJsonDelta { partial_json } => partial_json
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
                        .unwrap_or_default(),
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
                        cache_read_input_tokens: 0,
                        cache_creation_input_tokens: 0,
                        reasoning_output_tokens: 0,
                        cost_usd: None,
                        upstream_inference_cost_usd: None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use llm::prompt::{Message, Prompt, SystemBlock, UserBlock};
    use llm::{ModelChain, ModelSpec};

    fn base_prompt() -> Prompt {
        Prompt {
            chain: ModelChain::new("claude-sonnet-4"),
            system: vec![SystemBlock::Text { text: "sys".into() }],
            messages: vec![Message::User {
                content: vec![UserBlock::Text { text: "hi".into() }],
            }],
            tools: vec![],
            tool_choice: None,
            cache_key: None,
        }
    }

    fn base_spec() -> ModelSpec {
        ModelSpec::new("claude-sonnet-4").with_max_tokens(1024)
    }

    #[test]
    fn cache_marker_lands_on_last_user_block() {
        let req = prompt_to_request(&base_prompt(), &base_spec());
        let last = req.messages.last().unwrap();
        match &last.content[0] {
            AnthropicContent::Text { cache_control, .. } => {
                assert!(
                    cache_control.is_some(),
                    "expected cache_control on last user text block"
                );
            }
            _ => panic!("expected text content"),
        }
    }

    #[test]
    fn cache_marker_falls_through_to_system_when_no_messages() {
        let mut p = base_prompt();
        p.messages.clear();
        let req = prompt_to_request(&p, &base_spec());
        let sys = req.system.as_ref().unwrap().last().unwrap();
        assert!(sys.cache_control.is_some());
    }

    #[test]
    fn primary_model_id_is_used() {
        let req = prompt_to_request(&base_prompt(), &base_spec());
        assert_eq!(req.model, "claude-sonnet-4");
    }
}
