mod entity;
pub mod error;
pub mod repo;
mod thread;

use tracing::instrument;

pub use entity::*;
use error::AgentSessionError;
use primitives::{AgentId, UserMessageSource};
use repo::AgentSessionRepo;

es_entity::entity_id! { AgentSessionId }

pub use llm::RequestToolUse;

#[derive(Clone)]
pub struct Sessions {
    repo: AgentSessionRepo,
}

impl Sessions {
    pub fn new(pool: &sqlx::PgPool) -> Self {
        Self {
            repo: AgentSessionRepo::new(pool),
        }
    }

    #[instrument(name = "domain.agent_session.create_in_op", skip(self, op))]
    pub async fn create_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        agent_id: AgentId,
    ) -> Result<AgentSession, AgentSessionError> {
        let new_session = NewAgentSession::builder()
            .agent_id(agent_id)
            .build()
            .expect("NewAgentSession build");

        let mut session = self.repo.create_in_op(op, new_session).await?;
        let _ = session.init_initial_thread();
        self.repo.update_in_op(op, &mut session).await?;
        Ok(session)
    }

    #[instrument(
        name = "domain.agent_session.add_prompt_response",
        skip(self, response)
    )]
    pub async fn add_prompt_response(
        &self,
        agent_id: AgentId,
        response: llm::PromptResponse,
    ) -> Result<Vec<llm::RequestToolUse>, AgentSessionError> {
        let mut session = self.repo.find_by_agent_id(agent_id).await?;
        let next_tools = session.add_prompt_response(response);
        self.repo.update(&mut session).await?;
        Ok(next_tools)
    }

    #[instrument(name = "domain.agent_session.add_user_message", skip(self, prompt))]
    pub async fn add_user_message(
        &self,
        agent_id: AgentId,
        source: UserMessageSource,
        prompt: String,
    ) -> Result<Option<llm::Prompt>, AgentSessionError> {
        let mut session = self.repo.find_by_agent_id(agent_id).await?;
        match session.add_user_message(source, prompt)? {
            es_entity::Idempotent::Executed(prompt) => {
                self.repo.update(&mut session).await?;
                Ok(Some(prompt))
            }
            es_entity::Idempotent::AlreadyApplied => Ok(None),
        }
    }
}
