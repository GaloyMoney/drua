use serde::{Deserialize, Serialize};

use crate::content::*;
use crate::message::AgentMessage;

/// First line of a session JSONL file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionHeader {
    /// Always `"session"`.
    #[serde(rename = "type")]
    pub entry_type: SessionHeaderType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    #[serde(rename = "parentSession", skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
}

/// Literal discriminant for session header.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SessionHeaderType {
    #[serde(rename = "session")]
    Session,
}

/// Common fields for every session entry (lines 2+ of the JSONL file).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionEntryBase {
    /// 8-char hex identifier.
    pub id: String,
    /// Parent entry id, null for the first entry.
    #[serde(rename = "parentId")]
    pub parent_id: Option<String>,
    /// ISO 8601 timestamp.
    pub timestamp: String,
}

/// Session entry — discriminated union on `type` field.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEntry {
    Message(MessageEntry),
    Compaction(CompactionEntry),
    BranchSummary(BranchSummaryEntry),
    ModelChange(ModelChangeEntry),
    ThinkingLevelChange(ThinkingLevelChangeEntry),
    Custom(CustomEntry),
    CustomMessage(CustomMessageEntry),
    Label(LabelEntry),
    SessionInfo(SessionInfoEntry),
}

impl SessionEntry {
    pub fn base(&self) -> &SessionEntryBase {
        match self {
            Self::Message(e) => &e.base,
            Self::Compaction(e) => &e.base,
            Self::BranchSummary(e) => &e.base,
            Self::ModelChange(e) => &e.base,
            Self::ThinkingLevelChange(e) => &e.base,
            Self::Custom(e) => &e.base,
            Self::CustomMessage(e) => &e.base,
            Self::Label(e) => &e.base,
            Self::SessionInfo(e) => &e.base,
        }
    }
}

/// Wraps an `AgentMessage` as a session entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MessageEntry {
    #[serde(flatten)]
    pub base: SessionEntryBase,
    pub message: AgentMessage,
}

/// Context compaction record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompactionEntry {
    #[serde(flatten)]
    pub base: SessionEntryBase,
    pub summary: String,
    #[serde(rename = "firstKeptEntryId")]
    pub first_kept_entry_id: String,
    #[serde(rename = "tokensBefore")]
    pub tokens_before: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    #[serde(rename = "fromHook", skip_serializing_if = "Option::is_none")]
    pub from_hook: Option<bool>,
}

/// Summary of an abandoned conversation branch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BranchSummaryEntry {
    #[serde(flatten)]
    pub base: SessionEntryBase,
    #[serde(rename = "fromId")]
    pub from_id: String,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    #[serde(rename = "fromHook", skip_serializing_if = "Option::is_none")]
    pub from_hook: Option<bool>,
}

/// Model switch event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelChangeEntry {
    #[serde(flatten)]
    pub base: SessionEntryBase,
    pub provider: String,
    #[serde(rename = "modelId")]
    pub model_id: String,
}

/// Thinking level change event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThinkingLevelChangeEntry {
    #[serde(flatten)]
    pub base: SessionEntryBase,
    #[serde(rename = "thinkingLevel")]
    pub thinking_level: String,
}

/// Extension-stored data (not in LLM context).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CustomEntry {
    #[serde(flatten)]
    pub base: SessionEntryBase,
    #[serde(rename = "customType")]
    pub custom_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// Extension message that participates in LLM context.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CustomMessageEntry {
    #[serde(flatten)]
    pub base: SessionEntryBase,
    #[serde(rename = "customType")]
    pub custom_type: String,
    pub content: CustomContent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    pub display: bool,
}

/// Label applied to another entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LabelEntry {
    #[serde(flatten)]
    pub base: SessionEntryBase,
    #[serde(rename = "targetId")]
    pub target_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Session naming metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionInfoEntry {
    #[serde(flatten)]
    pub base: SessionEntryBase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// A line in a session JSONL file — either the header or an entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum FileEntry {
    Header(SessionHeader),
    Entry(Box<SessionEntry>),
}
