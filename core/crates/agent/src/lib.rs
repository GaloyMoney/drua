pub mod audit;
pub mod auth;
pub mod config;
mod entity;
pub mod error;
pub mod repo;
pub mod session;
pub mod toolset;

use tracing::instrument;

pub use config::{AgentsConfig, RoleConfig};
pub use entity::*;
use error::AgentError;
use primitives::{AgentId, AuthSubject, WorkspaceId};
use repo::AgentRepo;
use session::Sessions;

#[derive(Clone)]
pub struct Agents {
    repo: AgentRepo,
    sessions: Sessions,
    config: AgentsConfig,
    prompt_requests: llm::PromptRequestChannel,
}

impl Agents {
    pub fn new(
        pool: &sqlx::PgPool,
        config: AgentsConfig,
        prompt_requests: llm::PromptRequestChannel,
    ) -> Self {
        Self {
            repo: AgentRepo::new(pool),
            sessions: Sessions::new(pool),
            config,
            prompt_requests,
        }
    }

    #[instrument(name = "domain.agent.create", skip(self, tools))]
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        agent_role: AgentRole,
        name: impl Into<String> + std::fmt::Debug,
        tools: Vec<llm::prompt::Tool>,
    ) -> Result<Agent, AgentError> {
        let mut op = self.repo.begin_op().await?;
        let agent = self
            .create_in_op(&mut op, workspace_id, agent_role, name, tools)
            .await?;
        op.commit().await?;
        Ok(agent)
    }

    #[instrument(name = "domain.agent.create_in_op", skip(self, op, tools))]
    pub async fn create_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        workspace_id: WorkspaceId,
        agent_role: AgentRole,
        name: impl Into<String> + std::fmt::Debug,
        tools: Vec<llm::prompt::Tool>,
    ) -> Result<Agent, AgentError> {
        let role_config = self
            .config
            .builtin_roles
            .get(&agent_role)
            .ok_or(AgentError::RoleNotConfigured(agent_role))?
            .clone();

        let new_agent = NewAgent::builder()
            .workspace_id(workspace_id)
            .agent_role(agent_role)
            .name(name)
            .build()
            .expect("NewAgent build");

        let agent = self.repo.create_in_op(op, new_agent).await?;
        self.sessions
            .create_in_op(
                op,
                agent.id,
                role_config.model,
                role_config.system,
                tools,
                role_config.max_tokens,
            )
            .await?;
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
            tokio::spawn(async move {
                match response_rx.await {
                    Ok(Ok(response)) => {
                        let _ = sessions.add_prompt_response(id, response.clone()).await;
                        forward_response(response, &tx).await;
                    }
                    Ok(Err(e)) => {
                        let _ = tx
                            .send(AgentMessageEvent::Error {
                                message: e.to_string(),
                            })
                            .await;
                    }
                    Err(_) => {
                        // executor dropped the response channel without sending
                    }
                }
            });
        }

        Ok(rx)
    }
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
    let _ = tx
        .send(AgentMessageEvent::Done {
            turns: 1,
            input_tokens: response.usage.input_tokens,
            output_tokens: response.usage.output_tokens,
        })
        .await;
}
