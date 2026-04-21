//! Provider-agnostic streaming types and accumulator.
//!
//! [`StreamDelta`] represents a single incremental event from any LLM
//! provider. The agent loop forwards deltas to the UI in real-time and
//! feeds them into a [`StreamAccumulator`] that builds the final
//! [`PromptResponse`] once the stream completes.

use std::collections::HashMap;

use crate::prompt::AssistantBlock;
use crate::{PromptResponse, StopReason, Usage};

/// Provider-agnostic streaming delta. Forwarded to UI and fed to accumulator.
///
/// Providers emit only the events that map naturally to their wire format —
/// no lifecycle framing (start/stop) is required. The [`StreamAccumulator`]
/// infers block boundaries from the content deltas.
#[derive(Debug, Clone)]
pub enum StreamDelta {
    /// A chunk of assistant text content.
    TextDelta { text: String },
    /// A chunk of thinking / reasoning content.
    ThinkingDelta { text: String },
    /// Start of a new tool call. Must precede any [`ToolCallDelta`] for
    /// the same `id`.
    ToolCallStart { id: String, name: String },
    /// A chunk of JSON arguments for a tool call identified by `id`.
    ToolCallDelta { id: String, partial_json: String },
    /// Signature for the current thinking block.
    ThinkingSignature { signature: String },
    /// Token usage statistics. Accumulated additively — providers may emit
    /// this more than once (e.g. input tokens early, output tokens late).
    Usage {
        input_tokens: u32,
        output_tokens: u32,
    },
    /// The stream is complete.
    Done { stop_reason: Option<StopReason> },
    /// An error occurred mid-stream.
    Error { message: String },
}

/// Accumulates [`StreamDelta`] events into a complete [`PromptResponse`].
///
/// Block boundaries are inferred from the deltas themselves — no explicit
/// start/stop events are required from providers.
pub struct StreamAccumulator {
    thinking: Option<ThinkingBuilder>,
    text: Option<String>,
    tool_calls: Vec<ToolCallBuilder>,
    tool_call_index: HashMap<String, usize>,
    usage: Usage,
    stop_reason: Option<StopReason>,
    done: bool,
}

struct ThinkingBuilder {
    text: String,
    signature: Option<String>,
}

struct ToolCallBuilder {
    id: String,
    name: String,
    json_buf: String,
}

impl StreamAccumulator {
    pub fn new() -> Self {
        Self {
            thinking: None,
            text: None,
            tool_calls: Vec::new(),
            tool_call_index: HashMap::new(),
            usage: Usage::default(),
            stop_reason: None,
            done: false,
        }
    }

    /// Process a delta. The delta is consumed for accumulation.
    /// This does NOT return anything — the caller forwards the delta to UI
    /// separately.
    pub fn process(&mut self, delta: &StreamDelta) {
        match delta {
            StreamDelta::TextDelta { text } => {
                self.text.get_or_insert_with(String::new).push_str(text);
            }
            StreamDelta::ThinkingDelta { text } => {
                self.thinking
                    .get_or_insert_with(|| ThinkingBuilder {
                        text: String::new(),
                        signature: None,
                    })
                    .text
                    .push_str(text);
            }
            StreamDelta::ToolCallStart { id, name } => {
                let idx = self.tool_calls.len();
                self.tool_calls.push(ToolCallBuilder {
                    id: id.clone(),
                    name: name.clone(),
                    json_buf: String::new(),
                });
                self.tool_call_index.insert(id.clone(), idx);
            }
            StreamDelta::ToolCallDelta { id, partial_json } => {
                if let Some(&idx) = self.tool_call_index.get(id) {
                    self.tool_calls[idx].json_buf.push_str(partial_json);
                }
            }
            StreamDelta::ThinkingSignature { signature } => {
                if let Some(ref mut t) = self.thinking {
                    t.signature = Some(signature.clone());
                }
            }
            StreamDelta::Usage {
                input_tokens,
                output_tokens,
            } => {
                self.usage.input_tokens += *input_tokens;
                self.usage.output_tokens += *output_tokens;
            }
            StreamDelta::Done { stop_reason } => {
                self.stop_reason = *stop_reason;
                self.done = true;
            }
            StreamDelta::Error { .. } => {
                self.done = true;
            }
        }
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Produce the final response. Blocks are ordered: thinking, text, tool calls.
    pub fn finish(self) -> PromptResponse {
        let mut content = Vec::new();

        if let Some(t) = self.thinking {
            content.push(AssistantBlock::Thinking {
                text: t.text,
                signature: t.signature,
            });
        }

        if let Some(text) = self.text {
            content.push(AssistantBlock::Text {
                text,
                cache_control: None,
            });
        }

        for tc in self.tool_calls {
            let input = serde_json::from_str(&tc.json_buf).unwrap_or(serde_json::Value::Null);
            content.push(AssistantBlock::ToolUse {
                id: tc.id,
                name: tc.name,
                input,
                cache_control: None,
            });
        }

        PromptResponse {
            content,
            usage: self.usage,
            stop_reason: self.stop_reason,
        }
    }
}

impl Default for StreamAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates_text_only() {
        let mut acc = StreamAccumulator::new();
        acc.process(&StreamDelta::Usage {
            input_tokens: 10,
            output_tokens: 0,
        });
        acc.process(&StreamDelta::TextDelta {
            text: "Hello".to_string(),
        });
        acc.process(&StreamDelta::TextDelta {
            text: " world".to_string(),
        });
        acc.process(&StreamDelta::Usage {
            input_tokens: 0,
            output_tokens: 5,
        });
        acc.process(&StreamDelta::Done {
            stop_reason: Some(StopReason::EndTurn),
        });

        assert!(acc.is_done());
        let resp = acc.finish();
        assert_eq!(resp.content.len(), 1);
        match &resp.content[0] {
            AssistantBlock::Text { text, .. } => assert_eq!(text, "Hello world"),
            _ => panic!("expected text block"),
        }
        assert_eq!(resp.usage.input_tokens, 10);
        assert_eq!(resp.usage.output_tokens, 5);
        assert_eq!(resp.stop_reason, Some(StopReason::EndTurn));
    }

    #[test]
    fn accumulates_tool_use() {
        let mut acc = StreamAccumulator::new();
        acc.process(&StreamDelta::ToolCallStart {
            id: "tu_1".to_string(),
            name: "get_weather".to_string(),
        });
        acc.process(&StreamDelta::ToolCallDelta {
            id: "tu_1".to_string(),
            partial_json: r#"{"loc"#.to_string(),
        });
        acc.process(&StreamDelta::ToolCallDelta {
            id: "tu_1".to_string(),
            partial_json: r#"ation":"NYC"}"#.to_string(),
        });
        acc.process(&StreamDelta::Usage {
            input_tokens: 0,
            output_tokens: 20,
        });
        acc.process(&StreamDelta::Done {
            stop_reason: Some(StopReason::ToolUse),
        });

        let resp = acc.finish();
        assert_eq!(resp.content.len(), 1);
        match &resp.content[0] {
            AssistantBlock::ToolUse {
                id, name, input, ..
            } => {
                assert_eq!(id, "tu_1");
                assert_eq!(name, "get_weather");
                assert_eq!(input, &serde_json::json!({"location": "NYC"}));
            }
            _ => panic!("expected tool use block"),
        }
        assert_eq!(resp.stop_reason, Some(StopReason::ToolUse));
    }

    #[test]
    fn accumulates_thinking() {
        let mut acc = StreamAccumulator::new();
        acc.process(&StreamDelta::ThinkingDelta {
            text: "Let me think".to_string(),
        });
        acc.process(&StreamDelta::ThinkingDelta {
            text: " about this.".to_string(),
        });
        acc.process(&StreamDelta::ThinkingSignature {
            signature: "sig123".to_string(),
        });
        acc.process(&StreamDelta::Done {
            stop_reason: Some(StopReason::EndTurn),
        });

        let resp = acc.finish();
        assert_eq!(resp.content.len(), 1);
        match &resp.content[0] {
            AssistantBlock::Thinking { text, signature } => {
                assert_eq!(text, "Let me think about this.");
                assert_eq!(signature.as_deref(), Some("sig123"));
            }
            _ => panic!("expected thinking block"),
        }
    }

    #[test]
    fn accumulates_mixed_multi_block() {
        let mut acc = StreamAccumulator::new();
        acc.process(&StreamDelta::Usage {
            input_tokens: 15,
            output_tokens: 0,
        });
        // Thinking
        acc.process(&StreamDelta::ThinkingDelta {
            text: "hmm".to_string(),
        });
        // Text
        acc.process(&StreamDelta::TextDelta {
            text: "Hello".to_string(),
        });
        // Tool use
        acc.process(&StreamDelta::ToolCallStart {
            id: "tu_2".to_string(),
            name: "calc".to_string(),
        });
        acc.process(&StreamDelta::ToolCallDelta {
            id: "tu_2".to_string(),
            partial_json: r#"{"x":1}"#.to_string(),
        });
        acc.process(&StreamDelta::Usage {
            input_tokens: 0,
            output_tokens: 30,
        });
        acc.process(&StreamDelta::Done {
            stop_reason: Some(StopReason::ToolUse),
        });

        let resp = acc.finish();
        assert_eq!(resp.content.len(), 3);
        // Order: thinking, text, tool calls
        assert!(matches!(&resp.content[0], AssistantBlock::Thinking { .. }));
        assert!(matches!(&resp.content[1], AssistantBlock::Text { .. }));
        assert!(matches!(&resp.content[2], AssistantBlock::ToolUse { .. }));
    }

    #[test]
    fn error_mid_stream_marks_done() {
        let mut acc = StreamAccumulator::new();
        acc.process(&StreamDelta::TextDelta {
            text: "partial".to_string(),
        });
        acc.process(&StreamDelta::Error {
            message: "rate limited".to_string(),
        });

        assert!(acc.is_done());
        let resp = acc.finish();
        assert_eq!(resp.content.len(), 1);
        match &resp.content[0] {
            AssistantBlock::Text { text, .. } => assert_eq!(text, "partial"),
            _ => panic!("expected text block"),
        }
    }

    #[test]
    fn interleaved_tool_calls() {
        let mut acc = StreamAccumulator::new();
        acc.process(&StreamDelta::ToolCallStart {
            id: "call_a".to_string(),
            name: "search".to_string(),
        });
        acc.process(&StreamDelta::ToolCallStart {
            id: "call_b".to_string(),
            name: "fetch".to_string(),
        });
        // Interleaved deltas
        acc.process(&StreamDelta::ToolCallDelta {
            id: "call_a".to_string(),
            partial_json: r#"{"q":"#.to_string(),
        });
        acc.process(&StreamDelta::ToolCallDelta {
            id: "call_b".to_string(),
            partial_json: r#"{"url":"#.to_string(),
        });
        acc.process(&StreamDelta::ToolCallDelta {
            id: "call_a".to_string(),
            partial_json: r#""rust"}"#.to_string(),
        });
        acc.process(&StreamDelta::ToolCallDelta {
            id: "call_b".to_string(),
            partial_json: r#""https://example.com"}"#.to_string(),
        });
        acc.process(&StreamDelta::Done {
            stop_reason: Some(StopReason::ToolUse),
        });

        let resp = acc.finish();
        assert_eq!(resp.content.len(), 2);
        match &resp.content[0] {
            AssistantBlock::ToolUse {
                id, name, input, ..
            } => {
                assert_eq!(id, "call_a");
                assert_eq!(name, "search");
                assert_eq!(input, &serde_json::json!({"q": "rust"}));
            }
            _ => panic!("expected tool use block"),
        }
        match &resp.content[1] {
            AssistantBlock::ToolUse {
                id, name, input, ..
            } => {
                assert_eq!(id, "call_b");
                assert_eq!(name, "fetch");
                assert_eq!(input, &serde_json::json!({"url": "https://example.com"}));
            }
            _ => panic!("expected tool use block"),
        }
    }

    #[test]
    fn additive_usage_accumulation() {
        let mut acc = StreamAccumulator::new();
        // Anthropic pattern: input tokens early, output tokens late
        acc.process(&StreamDelta::Usage {
            input_tokens: 100,
            output_tokens: 0,
        });
        acc.process(&StreamDelta::TextDelta {
            text: "hi".to_string(),
        });
        acc.process(&StreamDelta::Usage {
            input_tokens: 0,
            output_tokens: 50,
        });
        acc.process(&StreamDelta::Done {
            stop_reason: Some(StopReason::EndTurn),
        });

        let resp = acc.finish();
        assert_eq!(resp.usage.input_tokens, 100);
        assert_eq!(resp.usage.output_tokens, 50);
    }
}
