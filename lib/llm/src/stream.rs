//! Provider-agnostic streaming deltas and a [`StreamAccumulator`] that
//! builds the final [`PromptResponse`] once the stream completes.

use std::collections::HashMap;

use crate::prompt::AssistantBlock;
use crate::{PromptResponse, StopReason, Usage};

/// Block boundaries are inferred from the deltas themselves; providers emit
/// only the events that map naturally to their wire format. `Usage` is
/// additive — providers may emit it more than once.
#[derive(Debug, Clone)]
pub enum StreamDelta {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        text: String,
    },
    /// Must precede any [`ToolCallDelta`] for the same `id`.
    ToolCallStart {
        id: String,
        name: String,
    },
    ToolCallDelta {
        id: String,
        partial_json: String,
    },
    ThinkingSignature {
        signature: String,
    },
    Usage {
        input_tokens: u32,
        output_tokens: u32,
        cache_read_input_tokens: u32,
        cache_creation_input_tokens: u32,
    },
    Done {
        stop_reason: Option<StopReason>,
    },
    Error {
        message: String,
    },
}

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

    /// The caller is responsible for forwarding the delta to UI separately.
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
                cache_read_input_tokens,
                cache_creation_input_tokens,
            } => {
                self.usage.input_tokens += *input_tokens;
                self.usage.output_tokens += *output_tokens;
                self.usage.cache_read_input_tokens += *cache_read_input_tokens;
                self.usage.cache_creation_input_tokens += *cache_creation_input_tokens;
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

    /// Block order: thinking, text, tool calls.
    pub fn finish(self) -> PromptResponse {
        let mut content = Vec::new();

        if let Some(t) = self.thinking {
            content.push(AssistantBlock::Thinking {
                text: t.text,
                signature: t.signature,
            });
        }

        if let Some(text) = self.text {
            content.push(AssistantBlock::Text { text });
        }

        for tc in self.tool_calls {
            // Zero-parameter tools receive no InputJsonDelta events; default
            // to "{}" so the Anthropic API doesn't reject a null input.
            let json_str = if tc.json_buf.is_empty() {
                "{}".to_string()
            } else {
                tc.json_buf
            };
            let input = serde_json::from_str(&json_str)
                .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
            content.push(AssistantBlock::ToolUse {
                id: tc.id,
                name: tc.name,
                input,
            });
        }

        PromptResponse {
            content,
            usage: self.usage,
            stop_reason: self.stop_reason,
            model_used: None,
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
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
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
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
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
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
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
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
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
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
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
            cache_read_input_tokens: 80,
            cache_creation_input_tokens: 20,
        });
        acc.process(&StreamDelta::TextDelta {
            text: "hi".to_string(),
        });
        acc.process(&StreamDelta::Usage {
            input_tokens: 0,
            output_tokens: 50,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        });
        acc.process(&StreamDelta::Done {
            stop_reason: Some(StopReason::EndTurn),
        });

        let resp = acc.finish();
        assert_eq!(resp.usage.input_tokens, 100);
        assert_eq!(resp.usage.output_tokens, 50);
        assert_eq!(resp.usage.cache_read_input_tokens, 80);
        assert_eq!(resp.usage.cache_creation_input_tokens, 20);
    }
}
