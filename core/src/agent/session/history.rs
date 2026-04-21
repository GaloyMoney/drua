use es_entity::EntityEvents;
use serde::Serialize;

use super::entity::{AgentSessionEvent, ThreadStartReason};
use super::message::{AssistantBlock, SandboxOperation, ToolResultInput};
use super::thread::{SessionThread as SessionThreadEntity, SessionThreadId};
use super::view::{MessageView, PromptDefinition};

// ─── Chat History (flat view) ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatHistoryRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatHistoryBlock {
    Text {
        text: String,
    },
    ToolUse {
        name: String,
    },
    Thinking {
        text: String,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
    SandboxNotification {
        sandbox_name: String,
        operation: SandboxNotificationOp,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SandboxNotificationOp {
    Attach { mode: String, mount_path: String },
    Detach,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatHistoryMessage {
    pub sequence: usize,
    pub role: ChatHistoryRole,
    pub blocks: Vec<ChatHistoryBlock>,
}

impl ChatHistoryBlock {
    fn from_assistant_block(block: &AssistantBlock) -> Self {
        match block {
            AssistantBlock::Text { text } => ChatHistoryBlock::Text { text: text.clone() },
            AssistantBlock::ToolUse { name, .. } => {
                ChatHistoryBlock::ToolUse { name: name.clone() }
            }
            AssistantBlock::Thinking { text, .. } => {
                ChatHistoryBlock::Thinking { text: text.clone() }
            }
        }
    }
}

// ─── Thread Graph ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThreadTurnState {
    User,
    Assistant,
    ToolUse,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStartReasonKind {
    InitialThread,
    ToolDefsUpdated,
    Compaction,
    Orphan,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionThreadInfo {
    pub id: SessionThreadId,
    pub is_current: bool,
    pub next_turn: ThreadTurnState,
    pub start_reason: ThreadStartReasonKind,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadMessage {
    pub role: ChatHistoryRole,
    pub blocks: Vec<ChatHistoryBlock>,
    /// Global MessageBlockIndex values for this message turn.
    /// Compare across threads to identify shared content nodes.
    pub block_indexes: Vec<usize>,
}

// ─── Builder functions (delegated from AgentSession) ─────────────────────────

pub(super) fn build_chat_history<'a>(
    events: impl Iterator<Item = &'a AgentSessionEvent>,
    last_n: usize,
) -> Vec<ChatHistoryMessage> {
    let mut messages = Vec::new();
    let mut seq = 0usize;

    for event in events {
        match event {
            AgentSessionEvent::UserInputAdded { text, .. } => {
                messages.push(ChatHistoryMessage {
                    sequence: seq,
                    role: ChatHistoryRole::User,
                    blocks: vec![ChatHistoryBlock::Text { text: text.clone() }],
                });
                seq += 1;
            }
            AgentSessionEvent::SandboxNotificationAdded {
                sandbox_name,
                operation,
                ..
            } => {
                messages.push(ChatHistoryMessage {
                    sequence: seq,
                    role: ChatHistoryRole::User,
                    blocks: vec![ChatHistoryBlock::SandboxNotification {
                        sandbox_name: sandbox_name.clone(),
                        operation: match operation {
                            SandboxOperation::Attach { mode, mount_path } => {
                                SandboxNotificationOp::Attach {
                                    mode: mode.clone(),
                                    mount_path: mount_path.clone(),
                                }
                            }
                            SandboxOperation::Detach => SandboxNotificationOp::Detach,
                        },
                    }],
                });
                seq += 1;
            }
            AgentSessionEvent::AssistantResponseReceived { content, .. } => {
                let blocks = content
                    .iter()
                    .map(ChatHistoryBlock::from_assistant_block)
                    .collect();
                messages.push(ChatHistoryMessage {
                    sequence: seq,
                    role: ChatHistoryRole::Assistant,
                    blocks,
                });
                seq += 1;
            }
            _ => {}
        }
    }

    let len = messages.len();
    if len > last_n {
        messages.split_off(len - last_n)
    } else {
        messages
    }
}

pub(super) fn build_thread_infos<'a>(
    threads: impl Iterator<Item = &'a SessionThreadEntity>,
    events: impl Iterator<Item = &'a AgentSessionEvent>,
    current_main_thread: Option<SessionThreadId>,
) -> Vec<SessionThreadInfo> {
    // Build a map of thread_id -> start_reason from session events
    let start_reasons: std::collections::HashMap<SessionThreadId, ThreadStartReason> = events
        .filter_map(|e| match e {
            AgentSessionEvent::ThreadStarted {
                thread_id,
                start_reason,
            } => Some((*thread_id, *start_reason)),
            _ => None,
        })
        .collect();

    threads
        .map(|thread| {
            let next_turn = if thread.is_user_turn() {
                ThreadTurnState::User
            } else if thread.is_assistant_turn() {
                ThreadTurnState::Assistant
            } else {
                ThreadTurnState::ToolUse
            };
            let start_reason = start_reasons
                .get(&thread.id)
                .map(|r| match r {
                    ThreadStartReason::InitialThread => ThreadStartReasonKind::InitialThread,
                    ThreadStartReason::ToolDefsUpdated => ThreadStartReasonKind::ToolDefsUpdated,
                    ThreadStartReason::Compaction { .. } => ThreadStartReasonKind::Compaction,
                    ThreadStartReason::Orphan { .. } => ThreadStartReasonKind::Orphan,
                })
                .unwrap_or(ThreadStartReasonKind::InitialThread);
            SessionThreadInfo {
                id: thread.id,
                is_current: current_main_thread == Some(thread.id),
                next_turn,
                start_reason,
            }
        })
        .collect()
}

/// Resolves a thread's prompt definition into a list of messages, each carrying
/// its own block indexes. The global block content list is built by scanning
/// session events, then each `MessageView` is resolved independently.
pub(super) fn build_thread_messages(
    prompt_def: PromptDefinition,
    events: &EntityEvents<AgentSessionEvent>,
) -> Vec<ThreadMessage> {
    // Build the global block content list from session events
    // (mirrors the event scan in PromptDefinition::into_prompt).
    let mut all_blocks: Vec<BlockContent> = Vec::new();

    for event in events.iter_all() {
        match event {
            AgentSessionEvent::UserInputAdded { text, .. } => {
                all_blocks.push(BlockContent::UserText(text.clone()));
            }
            AgentSessionEvent::SandboxNotificationAdded { text, .. } => {
                all_blocks.push(BlockContent::UserText(text.clone()));
            }
            AgentSessionEvent::AssistantResponseReceived { content, .. } => {
                for block in content {
                    all_blocks.push(BlockContent::AssistantBlock(block.clone()));
                }
            }
            AgentSessionEvent::ToolResultsAdded { results, .. } => {
                for result in results {
                    all_blocks.push(BlockContent::ToolResult(result.clone()));
                }
            }
            AgentSessionEvent::ToolResultsMasked { results, .. } => {
                for masked in results {
                    all_blocks.push(BlockContent::ToolResult(masked.replacement.clone()));
                }
            }
            _ => {}
        }
    }

    // Resolve each MessageView into a ThreadMessage with per-message block_indexes
    prompt_def
        .messages
        .iter()
        .map(|msg_view| resolve_message_view(msg_view, &all_blocks))
        .collect()
}

// ─── Private helpers ─────────────────────────────────────────────────────────

enum BlockContent {
    UserText(String),
    AssistantBlock(AssistantBlock),
    ToolResult(ToolResultInput),
}

fn resolve_message_view(view: &MessageView, all_blocks: &[BlockContent]) -> ThreadMessage {
    match view {
        MessageView::User(v) => {
            let block_indexes: Vec<usize> = v.indexes.iter().map(|i| i.index()).collect();
            let blocks = block_indexes
                .iter()
                .map(|&idx| match &all_blocks[idx] {
                    BlockContent::UserText(text) => ChatHistoryBlock::Text { text: text.clone() },
                    _ => panic!("User view index does not point to UserText"),
                })
                .collect();
            ThreadMessage {
                role: ChatHistoryRole::User,
                blocks,
                block_indexes,
            }
        }
        MessageView::Assistant(v) => {
            let block_indexes: Vec<usize> = v.indexes.iter().map(|i| i.index()).collect();
            let blocks = block_indexes
                .iter()
                .map(|&idx| match &all_blocks[idx] {
                    BlockContent::AssistantBlock(block) => {
                        ChatHistoryBlock::from_assistant_block(block)
                    }
                    _ => panic!("Assistant view index does not point to AssistantBlock"),
                })
                .collect();
            ThreadMessage {
                role: ChatHistoryRole::Assistant,
                blocks,
                block_indexes,
            }
        }
        MessageView::ToolResults(v) => {
            let block_indexes: Vec<usize> = v.indexes.iter().map(|i| i.index()).collect();
            let blocks = block_indexes
                .iter()
                .map(|&idx| match &all_blocks[idx] {
                    BlockContent::ToolResult(result) => ChatHistoryBlock::ToolResult {
                        tool_use_id: result.tool_use_id.clone(),
                        content: result.content.clone(),
                        is_error: result.is_error,
                    },
                    _ => panic!("ToolResults view index does not point to ToolResult"),
                })
                .collect();
            ThreadMessage {
                role: ChatHistoryRole::User,
                blocks,
                block_indexes,
            }
        }
    }
}
