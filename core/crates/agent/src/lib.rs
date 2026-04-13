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
}

impl Agents {
    pub fn new(pool: &sqlx::PgPool, prompt_requests: llm::PromptRequestChannel) -> Self {
        Self {
            repo: AgentRepo::new(pool),
            sessions: Sessions::new(pool),
            prompt_requests,
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

            let (request, mut response_rx) = llm::PromptRequest::new(prompt_state);
            self.prompt_requests
                .send(request)
                .await
                .map_err(|_| AgentError::PromptRequestChannelClosed)?;

            let sessions = self.sessions.clone();
            tokio::spawn(async move {
                while let Some(result) = response_rx.recv().await {
                    match result {
                        Ok(response) => {
                            for block in &response.content {
                                match block {
                                    llm::prompt::AssistantBlock::Text { text, .. } => {
                                        let _ = sessions
                                            .add_response_message(id, text.clone())
                                            .await;
                                        let _ = tx
                                            .send(AgentMessageEvent::AssistantText {
                                                text: text.clone(),
                                            })
                                            .await;
                                    }
                                    llm::prompt::AssistantBlock::ToolUse {
                                        name, input, ..
                                    } => {
                                        let _ = tx
                                            .send(AgentMessageEvent::ToolCall {
                                                name: name.clone(),
                                                arguments: Some(input.clone()),
                                            })
                                            .await;
                                    }
                                    llm::prompt::AssistantBlock::Thinking { .. } => {}
                                }
                            }
                            let _ = tx
                                .send(AgentMessageEvent::Done {
                                    turns: 1,
                                    input_tokens: response.usage.input_tokens,
                                    output_tokens: response.usage.output_tokens,
                                })
                                .await;
                        }
                        Err(e) => {
                            let _ = tx
                                .send(AgentMessageEvent::Error {
                                    message: e.to_string(),
                                })
                                .await;
                            break;
                        }
                    }
                }
            });
        }

        Ok(rx)
    }
}
