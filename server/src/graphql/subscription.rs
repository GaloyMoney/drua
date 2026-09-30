use async_graphql::{Context, Enum, SimpleObject, Subscription, Union};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;

use super::primitives::*;

/// Why a run-owned agent's dispatch loop stopped short of a natural
/// `Done`/no-`submit_output` outcome. `None` on `AssistantDoneEvent` for
/// every non-workflow (or unbounded) agent.
#[derive(Enum, Clone, Copy, PartialEq, Eq)]
pub enum BudgetStopKind {
    LimitReached,
    CostMeteringUnavailable,
}

impl From<drua_core::primitives::BudgetStopKind> for BudgetStopKind {
    fn from(kind: drua_core::primitives::BudgetStopKind) -> Self {
        match kind {
            drua_core::primitives::BudgetStopKind::LimitReached => Self::LimitReached,
            drua_core::primitives::BudgetStopKind::CostMeteringUnavailable => {
                Self::CostMeteringUnavailable
            }
        }
    }
}

#[derive(SimpleObject)]
pub struct UserMessageEvent {
    pub text: String,
}

#[derive(SimpleObject)]
pub struct AssistantTextEvent {
    pub text: String,
}

#[derive(SimpleObject)]
pub struct ThinkingEvent {
    pub text: String,
}

#[derive(SimpleObject)]
pub struct AssistantTextDeltaEvent {
    pub text: String,
}

#[derive(SimpleObject)]
pub struct ThinkingDeltaEvent {
    pub text: String,
}

#[derive(SimpleObject)]
pub struct ToolCallStartEvent {
    pub name: String,
}

#[derive(SimpleObject)]
pub struct ToolCallInputDeltaEvent {
    pub partial_json: String,
}

#[derive(SimpleObject)]
pub struct ToolResultEvent {
    pub name: String,
    pub is_error: bool,
    pub content: Option<String>,
}

#[derive(SimpleObject)]
pub struct AssistantDoneEvent {
    pub turns: u32,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub duration_ms: Option<u64>,
    pub cost_usd: Option<f64>,
    pub budget_stopped: Option<BudgetStopKind>,
}

#[derive(SimpleObject)]
pub struct ErrorEvent {
    pub message: String,
}

#[derive(SimpleObject)]
pub struct ServiceEvent {
    pub message: String,
}

#[derive(Union)]
pub enum ChatStreamEvent {
    UserMessage(UserMessageEvent),
    AssistantText(AssistantTextEvent),
    Thinking(ThinkingEvent),
    AssistantTextDelta(AssistantTextDeltaEvent),
    ThinkingDelta(ThinkingDeltaEvent),
    ToolCallStart(ToolCallStartEvent),
    ToolCallInputDelta(ToolCallInputDeltaEvent),
    ToolResult(ToolResultEvent),
    AssistantDone(AssistantDoneEvent),
    Error(ErrorEvent),
    Service(ServiceEvent),
}

impl From<drua_core::primitives::ChatOutputEvent> for ChatStreamEvent {
    fn from(event: drua_core::primitives::ChatOutputEvent) -> Self {
        use drua_core::primitives::ChatOutputEvent;
        match event {
            ChatOutputEvent::UserMessage { text, .. } => {
                Self::UserMessage(UserMessageEvent { text })
            }
            ChatOutputEvent::AssistantText { text } => {
                Self::AssistantText(AssistantTextEvent { text })
            }
            ChatOutputEvent::Thinking { text } => Self::Thinking(ThinkingEvent { text }),
            ChatOutputEvent::AssistantTextDelta { text } => {
                Self::AssistantTextDelta(AssistantTextDeltaEvent { text })
            }
            ChatOutputEvent::ThinkingDelta { text } => {
                Self::ThinkingDelta(ThinkingDeltaEvent { text })
            }
            ChatOutputEvent::ToolCallStart { name } => {
                Self::ToolCallStart(ToolCallStartEvent { name })
            }
            ChatOutputEvent::ToolCallInputDelta { partial_json } => {
                Self::ToolCallInputDelta(ToolCallInputDeltaEvent { partial_json })
            }
            ChatOutputEvent::ToolResult {
                name,
                is_error,
                content,
            } => Self::ToolResult(ToolResultEvent {
                name,
                is_error,
                content,
            }),
            ChatOutputEvent::AssistantDone {
                turns,
                input_tokens,
                output_tokens,
                duration_ms,
                cost_usd,
                budget_stopped,
            } => Self::AssistantDone(AssistantDoneEvent {
                turns,
                input_tokens,
                output_tokens,
                duration_ms,
                cost_usd,
                budget_stopped: budget_stopped.map(Into::into),
            }),
            ChatOutputEvent::Error { message } => Self::Error(ErrorEvent { message }),
            ChatOutputEvent::Service { message } => Self::Service(ServiceEvent { message }),
        }
    }
}

pub struct Subscription;

#[Subscription]
impl Subscription {
    /// Send a message to an agent and stream back response events.
    async fn agent_send_message(
        &self,
        ctx: &Context<'_>,
        agent_id: AgentId,
        prompt: String,
    ) -> async_graphql::Result<impl tokio_stream::Stream<Item = ChatStreamEvent>> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);

        let rx = app
            .agents()
            .send_message(sub.clone(), agent_id, prompt)
            .await?;

        Ok(ReceiverStream::new(rx).map(ChatStreamEvent::from))
    }
}
