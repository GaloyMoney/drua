mod entity;
pub mod error;
pub mod repo;
pub mod session;

use tracing::instrument;

pub use entity::*;
use error::AgentError;
use primitives::{AgentId, AuthSubject, WorkspaceId};
use repo::AgentRepo;
use session::Sessions;

#[derive(Clone)]
pub struct Agents {
    repo: AgentRepo,
    sessions: Sessions,
    prompt_requests: llm::PromptRequestChannel,
    #[allow(dead_code)]
    tool_uses: llm::ToolUseRequestChannel,
}

impl Agents {
    pub fn new(
        pool: &sqlx::PgPool,
        prompt_requests: llm::PromptRequestChannel,
        tool_uses: llm::ToolUseRequestChannel,
    ) -> Self {
        Self {
            repo: AgentRepo::new(pool),
            sessions: Sessions::new(pool),
            prompt_requests,
            tool_uses,
        }
    }

    #[instrument(name = "domain.agent.create", skip(self))]
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        agent_role: AgentRole,
        name: impl Into<String> + std::fmt::Debug,
    ) -> Result<Agent, AgentError> {
        let mut op = self.repo.begin_op().await?;
        let agent = self
            .create_in_op(&mut op, workspace_id, agent_role, name)
            .await?;
        op.commit().await?;
        Ok(agent)
    }

    #[instrument(name = "domain.agent.create_in_op", skip(self, op))]
    pub async fn create_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        workspace_id: WorkspaceId,
        agent_role: AgentRole,
        name: impl Into<String> + std::fmt::Debug,
    ) -> Result<Agent, AgentError> {
        let new_agent = NewAgent::builder()
            .workspace_id(workspace_id)
            .agent_role(agent_role)
            .name(name)
            .build()
            .expect("NewAgent build");

        let agent = self.repo.create_in_op(op, new_agent).await?;
        self.sessions.create_in_op(op, agent.id).await?;
        Ok(agent)
    }

    #[instrument(name = "domain.agent.send_message", skip(self, prompt))]
    pub async fn send_message(
        &self,
        subject: AuthSubject,
        id: AgentId,
        prompt: String,
    ) -> Result<tokio::sync::mpsc::Receiver<AgentMessageEvent>, AgentError> {
        let source = subject.to_message_source();
        let (tx, rx) = tokio::sync::mpsc::channel::<AgentMessageEvent>(64);

        if let Some(prompt_state) = self
            .sessions
            .add_user_message(id, source, prompt.clone())
            .await?
        {
            let _ = tx
                .send(AgentMessageEvent::UserMessage {
                    source,
                    text: prompt,
                })
                .await;

            let (request, response_rx) = llm::PromptRequest::new(prompt_state);
            self.prompt_requests
                .send(request)
                .await
                .map_err(|_| AgentError::PromptRequestChannelClosed)?;

            let sessions = self.sessions.clone();
            let tool_uses = self.tool_uses.clone();
            let prompt_requests = self.prompt_requests.clone();
            tokio::spawn(async move {
                let mut next = response_rx.await;
                let mut turn: u32 = 0;
                let mut input_tokens: u32 = 0;
                let mut output_tokens: u32 = 0;
                loop {
                    turn += 1;
                    let response = match next {
                        Ok(Ok(r)) => r,
                        Ok(Err(e)) => {
                            let _ = tx
                                .send(AgentMessageEvent::Error {
                                    message: e.to_string(),
                                })
                                .await;
                            return;
                        }
                        Err(_) => return, // executor dropped the response channel
                    };

                    input_tokens += response.usage.input_tokens;
                    output_tokens += response.usage.output_tokens;

                    let tool_calls = sessions
                        .add_prompt_response(id, response.clone())
                        .await
                        .unwrap_or_default();
                    forward_response(response, &tx).await;

                    if tool_calls.is_empty() {
                        break;
                    }

                    let results = fan_out_tool_calls(&tool_uses, tool_calls, &tx).await;

                    let updated_prompt = match sessions.add_tool_results(id, results).await {
                        Ok(p) => p,
                        Err(e) => {
                            let _ = tx
                                .send(AgentMessageEvent::Error {
                                    message: e.to_string(),
                                })
                                .await;
                            return;
                        }
                    };

                    let (request, response_rx) = llm::PromptRequest::new(updated_prompt);
                    if prompt_requests.send(request).await.is_err() {
                        let _ = tx
                            .send(AgentMessageEvent::Error {
                                message: "prompt request channel closed".to_string(),
                            })
                            .await;
                        return;
                    }
                    next = response_rx.await;
                }

                let _ = tx
                    .send(AgentMessageEvent::Done {
                        turns: turn,
                        input_tokens,
                        output_tokens,
                    })
                    .await;
            });
        }

        Ok(rx)
    }
}

async fn fan_out_tool_calls(
    tool_uses: &llm::ToolUseRequestChannel,
    calls: Vec<llm::RequestToolUse>,
    tx: &tokio::sync::mpsc::Sender<AgentMessageEvent>,
) -> Vec<llm::ToolUseResult> {
    let dispatches = calls.into_iter().map(|tu| {
        let chan = tool_uses.clone();
        async move {
            let id = tu.id.clone();
            let name = tu.name.clone();
            let (req, rx) = llm::ToolUseRequest::new(tu);
            let result = if chan.send(req).await.is_err() {
                llm::ToolUseResult {
                    tool_use_id: id,
                    content: "tool request channel closed".into(),
                    is_error: true,
                }
            } else {
                match rx.await {
                    Ok(Ok(r)) => r,
                    Ok(Err(e)) => llm::ToolUseResult {
                        tool_use_id: id,
                        content: e.to_string(),
                        is_error: true,
                    },
                    Err(_) => llm::ToolUseResult {
                        tool_use_id: id,
                        content: "tool response channel closed".into(),
                        is_error: true,
                    },
                }
            };
            (name, result)
        }
    });

    let outcomes = futures::future::join_all(dispatches).await;
    let mut results = Vec::with_capacity(outcomes.len());
    for (name, result) in outcomes {
        let _ = tx
            .send(AgentMessageEvent::ToolResult {
                name,
                is_error: result.is_error,
            })
            .await;
        results.push(result);
    }
    results
}

async fn forward_response(
    response: llm::PromptResponse,
    tx: &tokio::sync::mpsc::Sender<AgentMessageEvent>,
) {
    for block in response.content {
        match block {
            llm::prompt::AssistantBlock::Text { text, .. } => {
                let _ = tx.send(AgentMessageEvent::AssistantText { text }).await;
            }
            llm::prompt::AssistantBlock::ToolUse { name, input, .. } => {
                let _ = tx
                    .send(AgentMessageEvent::ToolCall {
                        name,
                        arguments: Some(input),
                    })
                    .await;
            }
            llm::prompt::AssistantBlock::Thinking { text, .. } => {
                let _ = tx.send(AgentMessageEvent::Thinking { text }).await;
            }
        }
    }
}
