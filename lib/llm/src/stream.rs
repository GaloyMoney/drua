//! Provider-agnostic streaming types and accumulator.
//!
//! [`StreamDelta`] represents a single incremental event from any LLM
//! provider. The agent loop forwards deltas to the UI in real-time and
//! feeds them into a [`StreamAccumulator`] that builds the final
//! [`PromptResponse`] once the stream completes.

use crate::prompt::AssistantBlock;
use crate::{PromptResponse, StopReason, Usage};

/// Provider-agnostic streaming delta. Forwarded to UI and fed to accumulator.
#[derive(Debug, Clone)]
pub enum StreamDelta {
    MessageStart {
        input_tokens: u32,
    },
    ContentBlockStart {
        index: usize,
        block_type: ContentBlockType,
    },
    TextDelta {
        index: usize,
        text: String,
    },
    ThinkingDelta {
        index: usize,
        text: String,
    },
    InputJsonDelta {
        index: usize,
        partial_json: String,
    },
    SignatureDelta {
        index: usize,
        signature: String,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageDelta {
        stop_reason: Option<StopReason>,
        output_tokens: u32,
    },
    MessageStop,
    Error {
        message: String,
    },
}

#[derive(Debug, Clone)]
pub enum ContentBlockType {
    Text,
    ToolUse { id: String, name: String },
    Thinking,
}

/// Accumulates [`StreamDelta`] events into a complete [`PromptResponse`].
pub struct StreamAccumulator {
    blocks: Vec<BlockBuilder>,
    usage: Usage,
    stop_reason: Option<StopReason>,
    done: bool,
}

enum BlockBuilder {
    Text(String),
    Thinking {
        text: String,
        signature: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        json_buf: String,
    },
}

impl StreamAccumulator {
    pub fn new() -> Self {
        Self {
            blocks: Vec::new(),
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
            StreamDelta::MessageStart { input_tokens } => {
                self.usage.input_tokens = *input_tokens;
            }
            StreamDelta::ContentBlockStart { block_type, .. } => match block_type {
                ContentBlockType::Text => self.blocks.push(BlockBuilder::Text(String::new())),
                ContentBlockType::ToolUse { id, name } => {
                    self.blocks.push(BlockBuilder::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        json_buf: String::new(),
                    });
                }
                ContentBlockType::Thinking => {
                    self.blocks.push(BlockBuilder::Thinking {
                        text: String::new(),
                        signature: None,
                    });
                }
            },
            StreamDelta::TextDelta { index, text } => {
                if let Some(BlockBuilder::Text(ref mut t)) = self.blocks.get_mut(*index) {
                    t.push_str(text);
                }
            }
            StreamDelta::ThinkingDelta { index, text } => {
                if let Some(BlockBuilder::Thinking {
                    text: ref mut t, ..
                }) = self.blocks.get_mut(*index)
                {
                    t.push_str(text);
                }
            }
            StreamDelta::InputJsonDelta {
                index,
                partial_json,
            } => {
                if let Some(BlockBuilder::ToolUse {
                    ref mut json_buf, ..
                }) = self.blocks.get_mut(*index)
                {
                    json_buf.push_str(partial_json);
                }
            }
            StreamDelta::SignatureDelta { index, signature } => {
                if let Some(BlockBuilder::Thinking {
                    signature: ref mut s,
                    ..
                }) = self.blocks.get_mut(*index)
                {
                    *s = Some(signature.clone());
                }
            }
            StreamDelta::ContentBlockStop { .. } => { /* blocks finalized in finish() */ }
            StreamDelta::MessageDelta {
                stop_reason,
                output_tokens,
            } => {
                self.stop_reason = *stop_reason;
                self.usage.output_tokens = *output_tokens;
            }
            StreamDelta::MessageStop => {
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

    pub fn finish(self) -> PromptResponse {
        let content = self
            .blocks
            .into_iter()
            .map(|b| match b {
                BlockBuilder::Text(text) => AssistantBlock::Text {
                    text,
                    cache_control: None,
                },
                BlockBuilder::Thinking { text, signature } => {
                    AssistantBlock::Thinking { text, signature }
                }
                BlockBuilder::ToolUse { id, name, json_buf } => {
                    let input = serde_json::from_str(&json_buf).unwrap_or(serde_json::Value::Null);
                    AssistantBlock::ToolUse {
                        id,
                        name,
                        input,
                        cache_control: None,
                    }
                }
            })
            .collect();
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
        acc.process(&StreamDelta::MessageStart { input_tokens: 10 });
        acc.process(&StreamDelta::ContentBlockStart {
            index: 0,
            block_type: ContentBlockType::Text,
        });
        acc.process(&StreamDelta::TextDelta {
            index: 0,
            text: "Hello".to_string(),
        });
        acc.process(&StreamDelta::TextDelta {
            index: 0,
            text: " world".to_string(),
        });
        acc.process(&StreamDelta::ContentBlockStop { index: 0 });
        acc.process(&StreamDelta::MessageDelta {
            stop_reason: Some(StopReason::EndTurn),
            output_tokens: 5,
        });
        acc.process(&StreamDelta::MessageStop);

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
        acc.process(&StreamDelta::MessageStart { input_tokens: 0 });
        acc.process(&StreamDelta::ContentBlockStart {
            index: 0,
            block_type: ContentBlockType::ToolUse {
                id: "tu_1".to_string(),
                name: "get_weather".to_string(),
            },
        });
        acc.process(&StreamDelta::InputJsonDelta {
            index: 0,
            partial_json: r#"{"loc"#.to_string(),
        });
        acc.process(&StreamDelta::InputJsonDelta {
            index: 0,
            partial_json: r#"ation":"NYC"}"#.to_string(),
        });
        acc.process(&StreamDelta::ContentBlockStop { index: 0 });
        acc.process(&StreamDelta::MessageDelta {
            stop_reason: Some(StopReason::ToolUse),
            output_tokens: 20,
        });
        acc.process(&StreamDelta::MessageStop);

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
        acc.process(&StreamDelta::MessageStart { input_tokens: 5 });
        acc.process(&StreamDelta::ContentBlockStart {
            index: 0,
            block_type: ContentBlockType::Thinking,
        });
        acc.process(&StreamDelta::ThinkingDelta {
            index: 0,
            text: "Let me think".to_string(),
        });
        acc.process(&StreamDelta::ThinkingDelta {
            index: 0,
            text: " about this.".to_string(),
        });
        acc.process(&StreamDelta::SignatureDelta {
            index: 0,
            signature: "sig123".to_string(),
        });
        acc.process(&StreamDelta::ContentBlockStop { index: 0 });
        acc.process(&StreamDelta::MessageDelta {
            stop_reason: Some(StopReason::EndTurn),
            output_tokens: 8,
        });
        acc.process(&StreamDelta::MessageStop);

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
        acc.process(&StreamDelta::MessageStart { input_tokens: 15 });
        // Block 0: Thinking
        acc.process(&StreamDelta::ContentBlockStart {
            index: 0,
            block_type: ContentBlockType::Thinking,
        });
        acc.process(&StreamDelta::ThinkingDelta {
            index: 0,
            text: "hmm".to_string(),
        });
        acc.process(&StreamDelta::ContentBlockStop { index: 0 });
        // Block 1: Text
        acc.process(&StreamDelta::ContentBlockStart {
            index: 1,
            block_type: ContentBlockType::Text,
        });
        acc.process(&StreamDelta::TextDelta {
            index: 1,
            text: "Hello".to_string(),
        });
        acc.process(&StreamDelta::ContentBlockStop { index: 1 });
        // Block 2: Tool use
        acc.process(&StreamDelta::ContentBlockStart {
            index: 2,
            block_type: ContentBlockType::ToolUse {
                id: "tu_2".to_string(),
                name: "calc".to_string(),
            },
        });
        acc.process(&StreamDelta::InputJsonDelta {
            index: 2,
            partial_json: r#"{"x":1}"#.to_string(),
        });
        acc.process(&StreamDelta::ContentBlockStop { index: 2 });
        acc.process(&StreamDelta::MessageDelta {
            stop_reason: Some(StopReason::ToolUse),
            output_tokens: 30,
        });
        acc.process(&StreamDelta::MessageStop);

        let resp = acc.finish();
        assert_eq!(resp.content.len(), 3);
        assert!(matches!(&resp.content[0], AssistantBlock::Thinking { .. }));
        assert!(matches!(&resp.content[1], AssistantBlock::Text { .. }));
        assert!(matches!(&resp.content[2], AssistantBlock::ToolUse { .. }));
    }

    #[test]
    fn error_mid_stream_marks_done() {
        let mut acc = StreamAccumulator::new();
        acc.process(&StreamDelta::MessageStart { input_tokens: 5 });
        acc.process(&StreamDelta::ContentBlockStart {
            index: 0,
            block_type: ContentBlockType::Text,
        });
        acc.process(&StreamDelta::TextDelta {
            index: 0,
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
}
