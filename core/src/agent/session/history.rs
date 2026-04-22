use es_entity::EntityEvents;
use serde::Serialize;

use super::entity::{AgentSessionEvent, ThreadStartReason};
use super::message::{AssistantBlock, SandboxOperation, ToolResultInput};
use super::metadata::AssistantResponseMetadata;
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
        input: serde_json::Value,
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
        /// The rendered `<sandbox>` XML injected into the prompt.
        text: String,
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
            AssistantBlock::ToolUse { name, input, .. } => ChatHistoryBlock::ToolUse {
                name: name.clone(),
                input: input.clone(),
            },
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

/// Token usage summary for an assistant response turn.
#[derive(Debug, Clone, Serialize)]
pub struct ThreadMessageUsage {
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub total_tokens: u64,
    pub total_cost: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadMessage {
    pub role: ChatHistoryRole,
    pub blocks: Vec<ChatHistoryBlock>,
    /// Global MessageBlockIndex values for this message turn.
    /// Compare across threads to identify shared content nodes.
    pub block_indexes: Vec<usize>,
    /// Usage metadata (only present for assistant messages).
    pub usage: Option<ThreadMessageUsage>,
    /// Timestamp of the event that produced this message.
    pub recorded_at: Option<chrono::DateTime<chrono::Utc>>,
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
                text,
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
                        text: text.clone(),
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
    // Build maps of thread_id -> (start_reason, creation_order) from session events.
    // Event order is the definitive creation order since iter_persisted() comes from
    // a HashMap whose iteration order is arbitrary.
    let mut start_reasons: std::collections::HashMap<SessionThreadId, ThreadStartReason> =
        std::collections::HashMap::new();
    let mut creation_order: std::collections::HashMap<SessionThreadId, usize> =
        std::collections::HashMap::new();
    let mut order_idx = 0usize;

    for event in events {
        if let AgentSessionEvent::ThreadStarted {
            thread_id,
            start_reason,
        } = event
        {
            start_reasons.insert(*thread_id, *start_reason);
            creation_order.insert(*thread_id, order_idx);
            order_idx += 1;
        }
    }

    let mut sorted_threads: Vec<_> = threads.collect();
    sorted_threads.sort_by_key(|t| creation_order.get(&t.id).copied().unwrap_or(usize::MAX));

    sorted_threads
        .into_iter()
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
///
/// For compacted threads whose views contain remapped replacement indexes,
/// this function reverse-maps them back to the original block indexes in the
/// GQL output so that both threads reference the same positions — enabling
/// positional alignment in the thread grid.
pub(super) fn build_thread_messages(
    prompt_def: PromptDefinition,
    events: &EntityEvents<AgentSessionEvent>,
) -> Vec<ThreadMessage> {
    // Build the global block content list from session events
    // (mirrors the event scan in PromptDefinition::into_prompt).
    let mut all_blocks: Vec<BlockContent> = Vec::new();
    // Map: block index → recorded_at timestamp.
    let mut block_timestamps: std::collections::HashMap<usize, chrono::DateTime<chrono::Utc>> =
        std::collections::HashMap::new();
    // Reverse map: replacement block index → original block index.
    // Used to report original positions in the GQL block_indexes output.
    let mut reverse_remap: std::collections::HashMap<usize, usize> =
        std::collections::HashMap::new();
    // Map: first block index of each assistant response → metadata.
    let mut assistant_metadata: std::collections::HashMap<usize, AssistantResponseMetadata> =
        std::collections::HashMap::new();

    for pe in events.iter_persisted() {
        let ts = pe.recorded_at;
        match &pe.event {
            AgentSessionEvent::UserInputAdded { text, .. } => {
                block_timestamps.insert(all_blocks.len(), ts);
                all_blocks.push(BlockContent::UserText(text.clone()));
            }
            AgentSessionEvent::SandboxNotificationAdded {
                sandbox_name,
                operation,
                text,
                ..
            } => {
                block_timestamps.insert(all_blocks.len(), ts);
                all_blocks.push(BlockContent::SandboxNotification {
                    sandbox_name: sandbox_name.clone(),
                    operation: operation.clone(),
                    text: text.clone(),
                });
            }
            AgentSessionEvent::AssistantResponseReceived {
                content, metadata, ..
            } => {
                let first_idx = all_blocks.len();
                for block in content {
                    block_timestamps.insert(all_blocks.len(), ts);
                    all_blocks.push(BlockContent::AssistantBlock(block.clone()));
                }
                assistant_metadata.insert(first_idx, metadata.clone());
            }
            AgentSessionEvent::ToolResultsAdded { results, .. } => {
                for result in results {
                    block_timestamps.insert(all_blocks.len(), ts);
                    all_blocks.push(BlockContent::ToolResult(result.clone()));
                }
            }
            AgentSessionEvent::ToolResultsMasked { results, .. } => {
                for masked in results {
                    let replacement_idx = all_blocks.len();
                    block_timestamps.insert(replacement_idx, ts);
                    reverse_remap.insert(replacement_idx, masked.original_index.index());
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
        .map(|msg_view| {
            resolve_message_view(
                msg_view,
                &all_blocks,
                &reverse_remap,
                &assistant_metadata,
                &block_timestamps,
            )
        })
        .collect()
}

// ─── Private helpers ─────────────────────────────────────────────────────────

enum BlockContent {
    UserText(String),
    SandboxNotification {
        sandbox_name: String,
        operation: SandboxOperation,
        text: String,
    },
    AssistantBlock(AssistantBlock),
    ToolResult(ToolResultInput),
}

fn resolve_message_view(
    view: &MessageView,
    all_blocks: &[BlockContent],
    reverse_remap: &std::collections::HashMap<usize, usize>,
    assistant_metadata: &std::collections::HashMap<usize, AssistantResponseMetadata>,
    block_timestamps: &std::collections::HashMap<usize, chrono::DateTime<chrono::Utc>>,
) -> ThreadMessage {
    match view {
        MessageView::User(v) => {
            let block_indexes: Vec<usize> = v.indexes.iter().map(|i| i.index()).collect();
            let recorded_at = block_indexes
                .first()
                .and_then(|idx| block_timestamps.get(idx).copied());
            let blocks = block_indexes
                .iter()
                .map(|&idx| match &all_blocks[idx] {
                    BlockContent::UserText(text) => ChatHistoryBlock::Text { text: text.clone() },
                    BlockContent::SandboxNotification {
                        sandbox_name,
                        operation,
                        text,
                    } => ChatHistoryBlock::SandboxNotification {
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
                        text: text.clone(),
                    },
                    _ => {
                        panic!("User view index does not point to UserText or SandboxNotification")
                    }
                })
                .collect();
            ThreadMessage {
                role: ChatHistoryRole::User,
                blocks,
                block_indexes,
                usage: None,
                recorded_at,
            }
        }
        MessageView::Assistant(v) => {
            let block_indexes: Vec<usize> = v.indexes.iter().map(|i| i.index()).collect();
            let recorded_at = block_indexes
                .first()
                .and_then(|idx| block_timestamps.get(idx).copied());
            let blocks = block_indexes
                .iter()
                .map(|&idx| match &all_blocks[idx] {
                    BlockContent::AssistantBlock(block) => {
                        ChatHistoryBlock::from_assistant_block(block)
                    }
                    _ => panic!("Assistant view index does not point to AssistantBlock"),
                })
                .collect();
            // Look up metadata by the first block index in this assistant turn.
            let usage = block_indexes.first().and_then(|&first_idx| {
                assistant_metadata
                    .get(&first_idx)
                    .map(|meta| ThreadMessageUsage {
                        model: meta.model.clone(),
                        input_tokens: meta.usage.input,
                        output_tokens: meta.usage.output,
                        cache_read_tokens: meta.usage.cache_read,
                        cache_write_tokens: meta.usage.cache_write,
                        total_tokens: meta.usage.total_tokens,
                        total_cost: meta.cost.total,
                    })
            });
            ThreadMessage {
                role: ChatHistoryRole::Assistant,
                blocks,
                block_indexes,
                usage,
                recorded_at,
            }
        }
        MessageView::ToolResults(v) => {
            // Resolve content from the actual view indexes (which may be
            // replacement indexes for compacted threads).
            let raw_indexes: Vec<usize> = v.indexes.iter().map(|i| i.index()).collect();
            let recorded_at = raw_indexes
                .first()
                .and_then(|idx| block_timestamps.get(idx).copied());
            let blocks = raw_indexes
                .iter()
                .map(|&idx| {
                    let result = match &all_blocks[idx] {
                        BlockContent::ToolResult(result) => result,
                        _ => panic!("ToolResults view index does not point to ToolResult"),
                    };
                    ChatHistoryBlock::ToolResult {
                        tool_use_id: result.tool_use_id.clone(),
                        content: result.content.clone(),
                        is_error: result.is_error,
                    }
                })
                .collect();
            // Reverse-map replacement indexes back to original positions for GQL
            // output so both threads reference the same column in the grid.
            let block_indexes = raw_indexes
                .iter()
                .map(|&idx| reverse_remap.get(&idx).copied().unwrap_or(idx))
                .collect();
            ThreadMessage {
                role: ChatHistoryRole::User,
                blocks,
                block_indexes,
                usage: None,
                recorded_at,
            }
        }
    }
}
