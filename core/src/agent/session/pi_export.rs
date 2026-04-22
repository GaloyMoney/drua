//! Pi-compatible JSONL session export (v3 format).
//!
//! The Pi session format uses newline-delimited JSON where:
//! - Line 1 is a [`PiSessionHeader`] with `type: "session"`, version 3
//! - Lines 2+ are [`PiEntry`] variants discriminated on `type`

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;

use super::{
    message::{AssistantBlock, ToolResultInput},
    metadata::AssistantResponseMetadata,
    AgentSessionId,
};

// ============================================================================
// Top-level JSONL types
// ============================================================================

/// Line 1 of every Pi session export.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PiSessionHeader {
    #[serde(rename = "type")]
    pub entry_type: &'static str,
    pub version: u32,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
}

/// Model-change entry emitted before messages.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PiModelChangeEntry {
    #[serde(rename = "type")]
    pub entry_type: &'static str,
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: String,
    pub provider: String,
    pub model_id: String,
}

/// Thinking-level-change entry emitted after model change.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PiThinkingLevelEntry {
    #[serde(rename = "type")]
    pub entry_type: &'static str,
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: String,
    pub thinking_level: String,
}

/// Message entry — wraps a single user, assistant, or tool-result message.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PiSessionEntry {
    #[serde(rename = "type")]
    pub entry_type: &'static str,
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: String,
    pub message: PiAgentMessage,
}

/// Unified enum for all Pi entries (excluding the header).
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum PiEntry {
    ModelChange(PiModelChangeEntry),
    ThinkingLevelChange(PiThinkingLevelEntry),
    Message(PiSessionEntry),
}

impl PiEntry {
    pub fn id(&self) -> &str {
        match self {
            PiEntry::ModelChange(e) => &e.id,
            PiEntry::ThinkingLevelChange(e) => &e.id,
            PiEntry::Message(e) => &e.id,
        }
    }
}

// ============================================================================
// Message types (discriminated on `role`)
// ============================================================================

/// A single message in Pi format, discriminated on `role`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum PiAgentMessage {
    #[serde(rename = "user")]
    User {
        content: Vec<PiContentBlock>,
        timestamp: i64,
    },
    #[serde(rename = "assistant", rename_all = "camelCase")]
    Assistant {
        content: Vec<PiContentBlock>,
        api: String,
        provider: String,
        model: String,
        usage: PiUsage,
        stop_reason: String,
        timestamp: i64,
        response_id: String,
    },
    #[serde(rename = "toolResult", rename_all = "camelCase")]
    ToolResult {
        tool_call_id: String,
        tool_name: String,
        content: Vec<PiContentBlock>,
        is_error: bool,
        timestamp: i64,
    },
}

/// Content block inside a message.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum PiContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "thinking")]
    Thinking {
        thinking: String,
        #[serde(rename = "thinkingSignature")]
        #[serde(skip_serializing_if = "Option::is_none")]
        thinking_signature: Option<String>,
    },
}

/// Token usage in Pi format.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PiUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total_tokens: u64,
    pub cost: PiCost,
}

/// Cost breakdown in Pi format (nested inside usage).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PiCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

// ============================================================================
// Helpers
// ============================================================================

fn format_ts(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub(super) fn ts_from_ms(base_ms: i64, offset: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(base_ms + offset).unwrap_or_default()
}

// ============================================================================
// Builders
// ============================================================================

/// Incrementing hex-ID generator for Pi entries.
pub(super) struct PiIdGenerator {
    counter: u32,
}

impl PiIdGenerator {
    pub fn new() -> Self {
        Self { counter: 0 }
    }

    /// Returns an 8-character hex string.
    pub fn next_id(&mut self) -> String {
        let id = format!("{:08x}", self.counter);
        self.counter += 1;
        id
    }
}

pub(super) fn build_header(session_id: AgentSessionId, now: DateTime<Utc>) -> PiSessionHeader {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    PiSessionHeader {
        entry_type: "session",
        version: 3,
        id: session_id.to_string(),
        timestamp: format_ts(now),
        cwd,
        parent_session: None,
    }
}

pub(super) fn build_model_change(
    id_gen: &mut PiIdGenerator,
    parent_id: Option<&str>,
    ts: DateTime<Utc>,
    model: &str,
) -> PiEntry {
    PiEntry::ModelChange(PiModelChangeEntry {
        entry_type: "model_change",
        id: id_gen.next_id(),
        parent_id: parent_id.map(String::from),
        timestamp: format_ts(ts),
        provider: "anthropic".to_string(),
        model_id: model.to_string(),
    })
}

pub(super) fn build_thinking_level_change(
    id_gen: &mut PiIdGenerator,
    parent_id: Option<&str>,
    ts: DateTime<Utc>,
    level: &str,
) -> PiEntry {
    PiEntry::ThinkingLevelChange(PiThinkingLevelEntry {
        entry_type: "thinking_level_change",
        id: id_gen.next_id(),
        parent_id: parent_id.map(String::from),
        timestamp: format_ts(ts),
        thinking_level: level.to_string(),
    })
}

pub(super) fn build_user_entry(
    id_gen: &mut PiIdGenerator,
    parent_id: Option<&str>,
    text: &str,
    ts: DateTime<Utc>,
) -> PiEntry {
    PiEntry::Message(PiSessionEntry {
        entry_type: "message",
        id: id_gen.next_id(),
        parent_id: parent_id.map(String::from),
        timestamp: format_ts(ts),
        message: PiAgentMessage::User {
            content: vec![PiContentBlock::Text {
                text: text.to_string(),
            }],
            timestamp: ts.timestamp_millis(),
        },
    })
}

pub(super) fn build_assistant_entry(
    id_gen: &mut PiIdGenerator,
    parent_id: Option<&str>,
    content: &[AssistantBlock],
    stop_reason: &str,
    metadata: &AssistantResponseMetadata,
    ts: DateTime<Utc>,
) -> PiEntry {
    let pi_content: Vec<PiContentBlock> = content
        .iter()
        .map(|block| match block {
            AssistantBlock::Text { text } => PiContentBlock::Text { text: text.clone() },
            AssistantBlock::ToolUse { id, name, input } => PiContentBlock::ToolUse {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            },
            AssistantBlock::Thinking { text, signature } => PiContentBlock::Thinking {
                thinking: text.clone(),
                thinking_signature: signature.clone(),
            },
        })
        .collect();

    let api = if metadata.api.is_empty() {
        "anthropic-messages".to_string()
    } else {
        metadata.api.clone()
    };

    PiEntry::Message(PiSessionEntry {
        entry_type: "message",
        id: id_gen.next_id(),
        parent_id: parent_id.map(String::from),
        timestamp: format_ts(ts),
        message: PiAgentMessage::Assistant {
            content: pi_content,
            api,
            provider: "anthropic".to_string(),
            model: metadata.model.clone(),
            usage: PiUsage {
                input: metadata.usage.input,
                output: metadata.usage.output,
                cache_read: metadata.usage.cache_read,
                cache_write: metadata.usage.cache_write,
                total_tokens: metadata.usage.total_tokens,
                cost: PiCost {
                    input: metadata.cost.input,
                    output: metadata.cost.output,
                    cache_read: metadata.cost.cache_read,
                    cache_write: metadata.cost.cache_write,
                    total: metadata.cost.total,
                },
            },
            stop_reason: stop_reason.to_string(),
            timestamp: ts.timestamp_millis(),
            response_id: String::new(),
        },
    })
}

#[allow(dead_code)]
pub(super) fn build_tool_result_entry(
    id_gen: &mut PiIdGenerator,
    parent_id: Option<&str>,
    result: &ToolResultInput,
    ts: DateTime<Utc>,
) -> PiEntry {
    PiEntry::Message(PiSessionEntry {
        entry_type: "message",
        id: id_gen.next_id(),
        parent_id: parent_id.map(String::from),
        timestamp: format_ts(ts),
        message: PiAgentMessage::ToolResult {
            tool_call_id: result.tool_use_id.clone(),
            tool_name: String::new(),
            content: vec![PiContentBlock::Text {
                text: result.content.clone(),
            }],
            is_error: result.is_error,
            timestamp: ts.timestamp_millis(),
        },
    })
}

// ============================================================================
// JSONL serialization
// ============================================================================

/// Serialize a header and entries into a JSONL string.
pub fn to_jsonl(header: &PiSessionHeader, entries: &[PiEntry]) -> String {
    let mut lines = Vec::with_capacity(1 + entries.len());
    lines.push(serde_json::to_string(header).expect("header serialization"));
    for entry in entries {
        lines.push(serde_json::to_string(entry).expect("entry serialization"));
    }
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_generator_produces_hex_ids() {
        let mut gen = PiIdGenerator::new();
        assert_eq!(gen.next_id(), "00000000");
        assert_eq!(gen.next_id(), "00000001");
        assert_eq!(gen.next_id(), "00000002");
    }

    #[test]
    fn header_serializes_as_session_type() {
        let header = PiSessionHeader {
            entry_type: "session",
            version: 3,
            id: "test-session".to_string(),
            timestamp: "2024-01-01T00:00:00.000Z".to_string(),
            cwd: "/tmp".to_string(),
            parent_session: None,
        };
        let json = serde_json::to_string(&header).unwrap();
        assert!(json.contains("\"type\":\"session\""));
        assert!(json.contains("\"version\":3"));
        assert!(json.contains("\"cwd\":\"/tmp\""));
        assert!(!json.contains("parentSession"));
    }

    #[test]
    fn user_message_content_is_array() {
        let msg = PiAgentMessage::User {
            content: vec![PiContentBlock::Text {
                text: "Hello".to_string(),
            }],
            timestamp: 1704067200000,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"role\":\"user\""));
        assert!(json.contains("\"content\":[{\"type\":\"text\",\"text\":\"Hello\"}]"));
    }

    #[test]
    fn assistant_message_has_pi_fields() {
        let msg = PiAgentMessage::Assistant {
            content: vec![PiContentBlock::Text {
                text: "Hi".to_string(),
            }],
            api: "anthropic-messages".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-opus-4-6".to_string(),
            usage: PiUsage {
                input: 100,
                output: 50,
                cache_read: 0,
                cache_write: 0,
                total_tokens: 150,
                cost: PiCost {
                    input: 0.005,
                    output: 0.00125,
                    cache_read: 0.0,
                    cache_write: 0.0,
                    total: 0.00625,
                },
            },
            stop_reason: "stop".to_string(),
            timestamp: 1704067200000,
            response_id: "msg_123".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"role\":\"assistant\""));
        assert!(json.contains("\"api\":\"anthropic-messages\""));
        assert!(json.contains("\"provider\":\"anthropic\""));
        assert!(json.contains("\"stopReason\":\"stop\""));
        assert!(json.contains("\"responseId\":\"msg_123\""));
        assert!(json.contains("\"totalTokens\":150"));
        assert!(json.contains("\"cacheRead\":0"));
        assert!(json.contains("\"cost\":{"));
    }

    #[test]
    fn tool_result_serializes_with_role() {
        let msg = PiAgentMessage::ToolResult {
            tool_call_id: "tc_1".to_string(),
            tool_name: "bash".to_string(),
            content: vec![PiContentBlock::Text {
                text: "output".to_string(),
            }],
            is_error: false,
            timestamp: 1704067200000,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"role\":\"toolResult\""));
        assert!(json.contains("\"toolCallId\":\"tc_1\""));
        assert!(json.contains("\"content\":[{\"type\":\"text\""));
    }

    #[test]
    fn model_change_entry_serializes() {
        let entry = PiModelChangeEntry {
            entry_type: "model_change",
            id: "00000000".to_string(),
            parent_id: None,
            timestamp: "2024-01-01T00:00:00.000Z".to_string(),
            provider: "anthropic".to_string(),
            model_id: "claude-opus-4-6".to_string(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"type\":\"model_change\""));
        assert!(json.contains("\"parentId\":null"));
        assert!(json.contains("\"modelId\":\"claude-opus-4-6\""));
    }

    #[test]
    fn thinking_level_change_entry_serializes() {
        let entry = PiThinkingLevelEntry {
            entry_type: "thinking_level_change",
            id: "00000001".to_string(),
            parent_id: Some("00000000".to_string()),
            timestamp: "2024-01-01T00:00:00.000Z".to_string(),
            thinking_level: "medium".to_string(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"type\":\"thinking_level_change\""));
        assert!(json.contains("\"parentId\":\"00000000\""));
        assert!(json.contains("\"thinkingLevel\":\"medium\""));
    }

    #[test]
    fn full_export_matches_pi_structure() {
        let header = PiSessionHeader {
            entry_type: "session",
            version: 3,
            id: "test-session".to_string(),
            timestamp: "2024-01-01T00:00:00.000Z".to_string(),
            cwd: "/tmp/test".to_string(),
            parent_session: None,
        };

        let model_change = PiEntry::ModelChange(PiModelChangeEntry {
            entry_type: "model_change",
            id: "00000000".to_string(),
            parent_id: None,
            timestamp: "2024-01-01T00:00:00.001Z".to_string(),
            provider: "anthropic".to_string(),
            model_id: "claude-opus-4-6".to_string(),
        });

        let thinking = PiEntry::ThinkingLevelChange(PiThinkingLevelEntry {
            entry_type: "thinking_level_change",
            id: "00000001".to_string(),
            parent_id: Some("00000000".to_string()),
            timestamp: "2024-01-01T00:00:00.002Z".to_string(),
            thinking_level: "medium".to_string(),
        });

        let user_msg = PiEntry::Message(PiSessionEntry {
            entry_type: "message",
            id: "00000002".to_string(),
            parent_id: Some("00000001".to_string()),
            timestamp: "2024-01-01T00:00:00.003Z".to_string(),
            message: PiAgentMessage::User {
                content: vec![PiContentBlock::Text {
                    text: "hello".to_string(),
                }],
                timestamp: 1704067200003,
            },
        });

        let entries = vec![model_change, thinking, user_msg];
        let jsonl = to_jsonl(&header, &entries);
        let lines: Vec<&str> = jsonl.trim().split('\n').collect();

        assert_eq!(lines.len(), 4);

        // Verify each line is valid JSON with correct type
        let h: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(h["type"], "session");
        assert_eq!(h["cwd"], "/tmp/test");

        let mc: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(mc["type"], "model_change");
        assert!(mc["parentId"].is_null());

        let tl: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(tl["type"], "thinking_level_change");
        assert_eq!(tl["parentId"], "00000000");

        let msg: serde_json::Value = serde_json::from_str(lines[3]).unwrap();
        assert_eq!(msg["type"], "message");
        assert_eq!(msg["message"]["role"], "user");
        assert!(msg["message"]["content"].is_array());
    }
}
