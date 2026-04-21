//! Pi-compatible JSONL session export (v3 format).
//!
//! The Pi session format uses newline-delimited JSON where:
//! - Line 1 is a [`PiSessionHeader`] with `type: "header"`, version 3
//! - Lines 2+ are [`PiSessionEntry`] variants discriminated on `type`

use chrono::{DateTime, Utc};
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
}

/// Lines 2+ in the export — currently only `message` entries.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PiSessionEntry {
    #[serde(rename = "type")]
    pub entry_type: &'static str,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub message: PiAgentMessage,
}

// ============================================================================
// Message types (discriminated on `role`)
// ============================================================================

/// A single message in Pi format, discriminated on `role`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum PiAgentMessage {
    #[serde(rename = "user")]
    User { content: String, timestamp: i64 },
    #[serde(rename = "assistant")]
    Assistant {
        content: Vec<PiContentBlock>,
        model: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
        usage: PiUsage,
        timestamp: i64,
    },
    #[serde(rename = "toolResult")]
    ToolResult {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        content: String,
        #[serde(rename = "isError")]
        is_error: bool,
        timestamp: i64,
    },
}

/// Content block inside an assistant message.
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
}

/// Token usage in Pi format.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PiUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
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
    PiSessionHeader {
        entry_type: "header",
        version: 3,
        id: session_id.to_string(),
        timestamp: now.to_rfc3339(),
        parent_session: None,
    }
}

pub(super) fn build_user_entry(
    id_gen: &mut PiIdGenerator,
    parent_id: Option<&str>,
    text: &str,
    timestamp_ms: i64,
) -> PiSessionEntry {
    PiSessionEntry {
        entry_type: "message",
        id: id_gen.next_id(),
        parent_id: parent_id.map(String::from),
        message: PiAgentMessage::User {
            content: text.to_string(),
            timestamp: timestamp_ms,
        },
    }
}

pub(super) fn build_assistant_entry(
    id_gen: &mut PiIdGenerator,
    parent_id: Option<&str>,
    content: &[AssistantBlock],
    stop_reason: &str,
    metadata: &AssistantResponseMetadata,
    timestamp_ms: i64,
) -> PiSessionEntry {
    let pi_content: Vec<PiContentBlock> = content
        .iter()
        .filter_map(|block| match block {
            AssistantBlock::Text { text } => Some(PiContentBlock::Text { text: text.clone() }),
            AssistantBlock::ToolUse { id, name, input } => Some(PiContentBlock::ToolUse {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            }),
            AssistantBlock::Thinking { .. } => None,
        })
        .collect();

    PiSessionEntry {
        entry_type: "message",
        id: id_gen.next_id(),
        parent_id: parent_id.map(String::from),
        message: PiAgentMessage::Assistant {
            content: pi_content,
            model: metadata.model.clone(),
            stop_reason: Some(stop_reason.to_string()),
            usage: PiUsage {
                input_tokens: metadata.usage.input,
                output_tokens: metadata.usage.output,
            },
            timestamp: timestamp_ms,
        },
    }
}

pub(super) fn build_tool_result_entry(
    id_gen: &mut PiIdGenerator,
    parent_id: Option<&str>,
    result: &ToolResultInput,
    timestamp_ms: i64,
) -> PiSessionEntry {
    PiSessionEntry {
        entry_type: "message",
        id: id_gen.next_id(),
        parent_id: parent_id.map(String::from),
        message: PiAgentMessage::ToolResult {
            tool_call_id: result.tool_use_id.clone(),
            tool_name: String::new(),
            content: result.content.clone(),
            is_error: result.is_error,
            timestamp: timestamp_ms,
        },
    }
}

// ============================================================================
// JSONL serialization
// ============================================================================

/// Serialize a header and entries into a JSONL string.
pub fn to_jsonl(header: &PiSessionHeader, entries: &[PiSessionEntry]) -> String {
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
    fn header_serializes_correctly() {
        let header = PiSessionHeader {
            entry_type: "header",
            version: 3,
            id: "test-session".to_string(),
            timestamp: "2024-01-01T00:00:00Z".to_string(),
            parent_session: None,
        };
        let json = serde_json::to_string(&header).unwrap();
        assert!(json.contains("\"type\":\"header\""));
        assert!(json.contains("\"version\":3"));
    }

    #[test]
    fn user_message_serializes_with_role() {
        let msg = PiAgentMessage::User {
            content: "Hello".to_string(),
            timestamp: 1704067200000,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"role\":\"user\""));
        assert!(json.contains("\"content\":\"Hello\""));
    }

    #[test]
    fn tool_result_serializes_with_role() {
        let msg = PiAgentMessage::ToolResult {
            tool_call_id: "tc_1".to_string(),
            tool_name: "bash".to_string(),
            content: "output".to_string(),
            is_error: false,
            timestamp: 1704067200000,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"role\":\"toolResult\""));
        assert!(json.contains("\"toolCallId\":\"tc_1\""));
    }
}
