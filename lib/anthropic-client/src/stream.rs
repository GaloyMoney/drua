//! Stream accumulator for Anthropic SSE events.
//!
//! Adapted from the Pi agent crate's `StreamState`. Processes streaming events
//! one at a time and accumulates text blocks, tool calls, thinking blocks, and
//! usage statistics into a final result that can be converted to `PromptResponse`.

use crate::types::{
    AnthropicContentBlock, AnthropicDelta, AnthropicDeltaUsage, AnthropicMessageDelta,
    AnthropicMessageStart, AnthropicStopReason, AnthropicStreamEvent,
};

// ============================================================================
// Accumulated content blocks (Pi-style internal types)
// ============================================================================

#[derive(Debug, Clone)]
pub(crate) enum AccumulatedBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        signature: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum AccumulatedStopReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    StopSequence,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct AccumulatedUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

/// Final accumulated result from processing a complete SSE stream.
#[derive(Debug)]
pub(crate) struct AccumulatedResponse {
    pub content: Vec<AccumulatedBlock>,
    pub usage: AccumulatedUsage,
    pub stop_reason: Option<AccumulatedStopReason>,
}

// ============================================================================
// Stream Accumulator
// ============================================================================

/// Stateful accumulator that processes `AnthropicStreamEvent`s and builds up
/// the final response. Mirrors the Pi agent crate's `StreamState` but without
/// the async stream machinery — we just call `process_event` for each SSE
/// event and then `finish` to get the result.
pub(crate) struct StreamAccumulator {
    content: Vec<AccumulatedBlock>,
    usage: AccumulatedUsage,
    stop_reason: Option<AccumulatedStopReason>,
    /// Buffer for tool-call JSON arguments being streamed incrementally.
    current_tool_json: String,
    current_tool_id: Option<String>,
    current_tool_name: Option<String>,
    /// Set to true once we've seen `MessageStop` or `Error`.
    done: bool,
    /// Error message from the API, if any.
    error_message: Option<String>,
}

impl StreamAccumulator {
    pub fn new() -> Self {
        Self {
            content: Vec::new(),
            usage: AccumulatedUsage::default(),
            stop_reason: None,
            current_tool_json: String::new(),
            current_tool_id: None,
            current_tool_name: None,
            done: false,
            error_message: None,
        }
    }

    /// Returns true if the stream is complete (MessageStop or Error received).
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Process a single SSE event's JSON data. Returns an error string if the
    /// API sent an error event.
    pub fn process_event(&mut self, data: &str) -> Result<(), String> {
        let event: AnthropicStreamEvent = serde_json::from_str(data)
            .map_err(|e| format!("JSON parse error: {e}\nData: {data}"))?;

        match event {
            AnthropicStreamEvent::MessageStart { message } => {
                self.handle_message_start(message);
            }
            AnthropicStreamEvent::ContentBlockStart {
                index: _,
                content_block,
            } => {
                self.handle_content_block_start(content_block);
            }
            AnthropicStreamEvent::ContentBlockDelta { index, delta } => {
                self.handle_content_block_delta(index, delta);
            }
            AnthropicStreamEvent::ContentBlockStop { index } => {
                self.handle_content_block_stop(index);
            }
            AnthropicStreamEvent::MessageDelta { delta, usage } => {
                self.handle_message_delta(&delta, usage);
            }
            AnthropicStreamEvent::MessageStop => {
                self.done = true;
            }
            AnthropicStreamEvent::Error { error } => {
                self.done = true;
                self.error_message = Some(error.message.clone());
                return Err(error.message);
            }
            AnthropicStreamEvent::Ping => {}
        }

        Ok(())
    }

    /// Consume the accumulator and return the final response.
    pub fn finish(self) -> AccumulatedResponse {
        AccumulatedResponse {
            content: self.content,
            usage: self.usage,
            stop_reason: self.stop_reason,
        }
    }

    fn handle_message_start(&mut self, message: AnthropicMessageStart) {
        if let Some(usage) = message.usage {
            self.usage.input_tokens = usage.input;
            self.usage.cache_read_tokens = usage.cache_read.unwrap_or(0);
            self.usage.cache_write_tokens = usage.cache_write.unwrap_or(0);
        }
    }

    fn handle_content_block_start(&mut self, content_block: AnthropicContentBlock) {
        match content_block {
            AnthropicContentBlock::Text => {
                self.content.push(AccumulatedBlock::Text {
                    text: String::new(),
                });
            }
            AnthropicContentBlock::Thinking => {
                self.content.push(AccumulatedBlock::Thinking {
                    thinking: String::new(),
                    signature: None,
                });
            }
            AnthropicContentBlock::ToolUse { id, name } => {
                self.current_tool_json.clear();
                self.current_tool_id = id;
                self.current_tool_name = name;
                self.content.push(AccumulatedBlock::ToolUse {
                    id: self.current_tool_id.clone().unwrap_or_default(),
                    name: self.current_tool_name.clone().unwrap_or_default(),
                    arguments: serde_json::Value::Null,
                });
            }
        }
    }

    fn handle_content_block_delta(&mut self, index: u32, delta: AnthropicDelta) {
        let idx = index as usize;

        match delta {
            AnthropicDelta::TextDelta { text } => {
                if let Some(text) = text {
                    if let Some(AccumulatedBlock::Text { text: ref mut t }) =
                        self.content.get_mut(idx)
                    {
                        t.push_str(&text);
                    }
                }
            }
            AnthropicDelta::ThinkingDelta { thinking } => {
                if let Some(thinking) = thinking {
                    if let Some(AccumulatedBlock::Thinking {
                        thinking: ref mut t,
                        ..
                    }) = self.content.get_mut(idx)
                    {
                        t.push_str(&thinking);
                    }
                }
            }
            AnthropicDelta::InputJsonDelta { partial_json } => {
                if let Some(partial_json) = partial_json {
                    self.current_tool_json.push_str(&partial_json);
                }
            }
            AnthropicDelta::SignatureDelta { signature } => {
                if let Some(sig) = signature {
                    if let Some(AccumulatedBlock::Thinking {
                        signature: ref mut s,
                        ..
                    }) = self.content.get_mut(idx)
                    {
                        *s = Some(sig);
                    }
                }
            }
        }
    }

    fn handle_content_block_stop(&mut self, index: u32) {
        let idx = index as usize;

        if let Some(AccumulatedBlock::ToolUse {
            ref mut arguments, ..
        }) = self.content.get_mut(idx)
        {
            // Parse the accumulated JSON string into a Value.
            *arguments = match serde_json::from_str(&self.current_tool_json) {
                Ok(args) => args,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        raw = %self.current_tool_json,
                        "Failed to parse tool arguments as JSON"
                    );
                    serde_json::Value::Null
                }
            };
            self.current_tool_json.clear();
        }
    }

    fn handle_message_delta(
        &mut self,
        delta: &AnthropicMessageDelta,
        usage: Option<AnthropicDeltaUsage>,
    ) {
        if let Some(stop_reason) = delta.stop_reason {
            self.stop_reason = Some(match stop_reason {
                AnthropicStopReason::EndTurn => AccumulatedStopReason::EndTurn,
                AnthropicStopReason::MaxTokens => AccumulatedStopReason::MaxTokens,
                AnthropicStopReason::ToolUse => AccumulatedStopReason::ToolUse,
                AnthropicStopReason::StopSequence => AccumulatedStopReason::StopSequence,
            });
        }

        if let Some(u) = usage {
            self.usage.output_tokens = u.output_tokens;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates_text_block() {
        let mut acc = StreamAccumulator::new();

        // message_start
        acc.process_event(r#"{"type":"message_start","message":{"usage":{"input_tokens":10}}}"#)
            .unwrap();

        // content_block_start (text)
        acc.process_event(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text"}}"#,
        )
        .unwrap();

        // text deltas
        acc.process_event(r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#)
            .unwrap();
        acc.process_event(r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world"}}"#)
            .unwrap();

        // content_block_stop
        acc.process_event(r#"{"type":"content_block_stop","index":0}"#)
            .unwrap();

        // message_delta with stop reason
        acc.process_event(r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#)
            .unwrap();

        // message_stop
        acc.process_event(r#"{"type":"message_stop"}"#).unwrap();

        assert!(acc.is_done());
        let result = acc.finish();

        assert_eq!(result.content.len(), 1);
        match &result.content[0] {
            AccumulatedBlock::Text { text } => assert_eq!(text, "Hello world"),
            _ => panic!("expected text block"),
        }
        assert_eq!(result.usage.input_tokens, 10);
        assert_eq!(result.usage.output_tokens, 5);
        assert!(matches!(
            result.stop_reason,
            Some(AccumulatedStopReason::EndTurn)
        ));
    }

    #[test]
    fn accumulates_tool_use() {
        let mut acc = StreamAccumulator::new();

        acc.process_event(r#"{"type":"message_start","message":{}}"#)
            .unwrap();
        acc.process_event(r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tu_1","name":"get_weather"}}"#)
            .unwrap();
        acc.process_event(r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"loc"}}"#)
            .unwrap();
        acc.process_event(r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"ation\":\"NYC\"}"}}"#)
            .unwrap();
        acc.process_event(r#"{"type":"content_block_stop","index":0}"#)
            .unwrap();
        acc.process_event(r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":20}}"#)
            .unwrap();
        acc.process_event(r#"{"type":"message_stop"}"#).unwrap();

        let result = acc.finish();
        assert_eq!(result.content.len(), 1);
        match &result.content[0] {
            AccumulatedBlock::ToolUse {
                id,
                name,
                arguments,
            } => {
                assert_eq!(id, "tu_1");
                assert_eq!(name, "get_weather");
                assert_eq!(arguments, &serde_json::json!({"location": "NYC"}));
            }
            _ => panic!("expected tool use block"),
        }
        assert!(matches!(
            result.stop_reason,
            Some(AccumulatedStopReason::ToolUse)
        ));
    }

    #[test]
    fn handles_api_error() {
        let mut acc = StreamAccumulator::new();
        let result = acc.process_event(r#"{"type":"error","error":{"message":"rate limited"}}"#);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "rate limited");
        assert!(acc.is_done());
    }
}
