use async_graphql::{ComplexObject, Context, Enum, Object, SimpleObject, Union};

use drua_core::agent::session::history;

use super::primitives::*;

#[derive(Enum, Clone, Copy, PartialEq, Eq)]
pub enum ChatMessageRole {
    User,
    Assistant,
}

#[derive(Enum, Clone, Copy, PartialEq, Eq)]
pub enum ThreadTurn {
    User,
    Assistant,
    ToolUse,
}

#[derive(Enum, Clone, Copy, PartialEq, Eq)]
pub enum ThreadStartReason {
    InitialThread,
    ToolDefsUpdated,
    ContextRefreshed,
    Compaction,
    Orphan,
}

#[derive(Enum, Clone, Copy, PartialEq, Eq)]
pub enum SystemBlockKind {
    Base,
    Tools,
    Behavioral,
    Role,
    Notes,
    Skills,
}

#[derive(Union, Clone)]
pub enum ChatContentBlock {
    Text(TextContent),
    ToolUse(ToolUseContent),
    Thinking(ThinkingContent),
    ToolResult(ToolResultContent),
    SandboxNotification(SandboxNotificationContent),
}

#[derive(SimpleObject, Clone)]
pub struct TextContent {
    pub text: String,
}

#[derive(SimpleObject, Clone)]
pub struct ToolUseContent {
    pub name: String,
    pub input: String,
}

#[derive(SimpleObject, Clone)]
pub struct ThinkingContent {
    pub text: String,
}

#[derive(SimpleObject, Clone)]
pub struct ToolResultContent {
    pub tool_use_id: String,
    pub content: String,
    pub is_error: bool,
}

#[derive(SimpleObject, Clone)]
pub struct SandboxNotificationContent {
    pub sandbox_name: String,
    pub operation: SandboxNotificationOperation,
    pub mode: Option<String>,
    /// The rendered notification text that was injected into the prompt.
    pub text: String,
}

#[derive(Enum, Clone, Copy, PartialEq, Eq)]
pub enum SandboxNotificationOperation {
    Attach,
    Detach,
}

#[derive(SimpleObject, Clone)]
pub struct ChatMessage {
    /// Ordinal position in the full message history (0-based).
    pub sequence: i32,
    pub role: ChatMessageRole,
    pub content: Vec<ChatContentBlock>,
}

#[derive(SimpleObject, Clone)]
#[graphql(complex)]
pub struct SessionThread {
    pub id: SessionThreadId,
    /// Whether this is the currently active thread.
    pub is_current: bool,
    /// Whose turn it is next on this thread.
    pub next_turn: ThreadTurn,
    /// Why this thread was created.
    pub start_reason: ThreadStartReason,

    /// The agent that owns this session (used internally to resolve messages).
    #[graphql(skip)]
    agent_id: AgentId,
}

#[ComplexObject]
impl SessionThread {
    /// Resolved messages for this thread (as the LLM sees them).
    /// Each entry carries its own block indexes for shared-node detection.
    async fn messages(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<ThreadMessageEntry>> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let messages = app
            .agents()
            .thread_messages(sub, self.agent_id, self.id)
            .await?;
        Ok(messages.into_iter().map(ThreadMessageEntry::from).collect())
    }

    /// System prompt blocks referenced by this thread, in canonical order.
    /// Each entry carries its global SystemBlockIndex so the TUI can detect
    /// shared columns across threads (same idx → same content).
    async fn system_blocks(
        &self,
        ctx: &Context<'_>,
    ) -> async_graphql::Result<Vec<SystemBlockInfo>> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let view = app
            .agents()
            .thread_system_view(sub, self.agent_id, self.id)
            .await?;
        Ok(view
            .system_blocks
            .into_iter()
            .map(SystemBlockInfo::from)
            .collect())
    }

    /// Number of tool definitions exposed to the LLM on this thread.
    async fn tool_definitions_count(&self, ctx: &Context<'_>) -> async_graphql::Result<i32> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let view = app
            .agents()
            .thread_system_view(sub, self.agent_id, self.id)
            .await?;
        Ok(view.tool_definitions_count as i32)
    }
}

#[derive(SimpleObject, Clone)]
pub struct SystemBlockInfo {
    /// Global SystemBlockIndex (position in the per-session flat list of all
    /// system blocks ever pushed). Equal indexes across threads = same content.
    pub index: i32,
    pub kind: SystemBlockKind,
    pub text: String,
}

#[derive(SimpleObject, Clone)]
pub struct ThreadMessageEntry {
    pub role: ChatMessageRole,
    pub content: Vec<ChatContentBlock>,
    /// Global block indexes referenced by this message turn.
    /// Compare across threads to identify shared content nodes.
    pub block_indexes: Vec<i32>,
    /// Token usage metadata (only present for assistant messages).
    pub usage: Option<ThreadMessageUsage>,
    /// Timestamp of the event that produced this message.
    pub recorded_at: Option<Timestamp>,
}

#[derive(SimpleObject, Clone)]
pub struct ThreadMessageUsage {
    pub model: String,
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub cache_read_tokens: i32,
    pub cache_write_tokens: i32,
    pub total_tokens: i32,
    /// Total cost in USD.
    pub total_cost: f64,
}

pub struct AgentSession {
    pub(super) agent_id: AgentId,
}

#[Object]
impl AgentSession {
    /// Flat chat history: recent user messages and assistant responses.
    /// Returns the last `last` messages in chronological order.
    async fn chat_history(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 20)] last: i32,
    ) -> async_graphql::Result<Vec<ChatMessage>> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let last_n = last.max(1) as usize;
        let messages = app
            .agents()
            .chat_history(sub, self.agent_id, last_n)
            .await?;
        Ok(messages.into_iter().map(ChatMessage::from).collect())
    }

    /// All threads in this session.
    async fn threads(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<SessionThread>> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let infos = app.agents().thread_infos(sub, self.agent_id).await?;
        Ok(infos
            .into_iter()
            .map(|info| SessionThread::from_info(info, self.agent_id))
            .collect())
    }
}

impl From<history::ChatHistoryMessage> for ChatMessage {
    fn from(m: history::ChatHistoryMessage) -> Self {
        Self {
            sequence: m.sequence as i32,
            role: match m.role {
                history::ChatHistoryRole::User => ChatMessageRole::User,
                history::ChatHistoryRole::Assistant => ChatMessageRole::Assistant,
            },
            content: m.blocks.into_iter().map(ChatContentBlock::from).collect(),
        }
    }
}

impl From<history::ChatHistoryBlock> for ChatContentBlock {
    fn from(b: history::ChatHistoryBlock) -> Self {
        match b {
            history::ChatHistoryBlock::Text { text } => {
                ChatContentBlock::Text(TextContent { text })
            }
            history::ChatHistoryBlock::ToolUse { name, input } => {
                ChatContentBlock::ToolUse(ToolUseContent {
                    name,
                    input: serde_json::to_string_pretty(&input).unwrap_or_default(),
                })
            }
            history::ChatHistoryBlock::Thinking { text } => {
                ChatContentBlock::Thinking(ThinkingContent { text })
            }
            history::ChatHistoryBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => ChatContentBlock::ToolResult(ToolResultContent {
                tool_use_id,
                content,
                is_error,
            }),
            history::ChatHistoryBlock::SandboxNotification {
                sandbox_name,
                operation,
                text,
            } => {
                let (op, mode) = match operation {
                    history::SandboxNotificationOp::Attach { mode, .. } => {
                        (SandboxNotificationOperation::Attach, Some(mode))
                    }
                    history::SandboxNotificationOp::Detach => {
                        (SandboxNotificationOperation::Detach, None)
                    }
                };
                ChatContentBlock::SandboxNotification(SandboxNotificationContent {
                    sandbox_name,
                    operation: op,
                    mode,
                    text,
                })
            }
        }
    }
}

impl SessionThread {
    fn from_info(info: history::SessionThreadInfo, agent_id: AgentId) -> Self {
        Self {
            id: info.id,
            is_current: info.is_current,
            next_turn: match info.next_turn {
                history::ThreadTurnState::User => ThreadTurn::User,
                history::ThreadTurnState::Assistant => ThreadTurn::Assistant,
                history::ThreadTurnState::ToolUse => ThreadTurn::ToolUse,
            },
            start_reason: match info.start_reason {
                history::ThreadStartReasonKind::InitialThread => ThreadStartReason::InitialThread,
                history::ThreadStartReasonKind::ToolDefsUpdated => {
                    ThreadStartReason::ToolDefsUpdated
                }
                history::ThreadStartReasonKind::ContextRefreshed => {
                    ThreadStartReason::ContextRefreshed
                }
                history::ThreadStartReasonKind::Compaction => ThreadStartReason::Compaction,
                history::ThreadStartReasonKind::Orphan => ThreadStartReason::Orphan,
            },
            agent_id,
        }
    }
}

impl From<history::ThreadSystemBlock> for SystemBlockInfo {
    fn from(b: history::ThreadSystemBlock) -> Self {
        Self {
            index: b.index as i32,
            kind: match b.kind {
                history::ThreadSystemBlockKind::Base => SystemBlockKind::Base,
                history::ThreadSystemBlockKind::Tools => SystemBlockKind::Tools,
                history::ThreadSystemBlockKind::Behavioral => SystemBlockKind::Behavioral,
                history::ThreadSystemBlockKind::Role => SystemBlockKind::Role,
                history::ThreadSystemBlockKind::Notes => SystemBlockKind::Notes,
                history::ThreadSystemBlockKind::Skills => SystemBlockKind::Skills,
            },
            text: b.text,
        }
    }
}

impl From<history::ThreadMessage> for ThreadMessageEntry {
    fn from(m: history::ThreadMessage) -> Self {
        Self {
            role: match m.role {
                history::ChatHistoryRole::User => ChatMessageRole::User,
                history::ChatHistoryRole::Assistant => ChatMessageRole::Assistant,
            },
            content: m.blocks.into_iter().map(ChatContentBlock::from).collect(),
            block_indexes: m.block_indexes.into_iter().map(|i| i as i32).collect(),
            usage: m.usage.map(|u| ThreadMessageUsage {
                model: u.model,
                input_tokens: u.input_tokens as i32,
                output_tokens: u.output_tokens as i32,
                cache_read_tokens: u.cache_read_tokens as i32,
                cache_write_tokens: u.cache_write_tokens as i32,
                total_tokens: u.total_tokens as i32,
                total_cost: u.total_cost,
            }),
            recorded_at: m.recorded_at.map(Timestamp::from),
        }
    }
}
