mod entity;
pub mod error;
mod message;
mod new_entity;
mod new_thread;
pub mod repo;
mod thread;

use tracing::instrument;

use crate::primitives::{AgentId, UserMessageSource};
pub use entity::*;
use error::AgentSessionError;
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

    #[instrument(
        name = "domain.agent_session.create_in_op",
        skip(self, op, system, tools)
    )]
    #[allow(clippy::too_many_arguments)]
    pub async fn create_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        agent_id: AgentId,
        model: impl Into<String> + std::fmt::Debug,
        system: Vec<llm::prompt::SystemBlock>,
        tools: Vec<llm::prompt::Tool>,
        max_tokens: u32,
        reset_time_delta_seconds: Option<crate::agent::ResetTimeDeltaSeconds>,
    ) -> Result<AgentSession, AgentSessionError> {
        let new_session = NewAgentSession::builder()
            .agent_id(agent_id)
            .model(model)
            .system(system)
            .tools(tools)
            .max_tokens(max_tokens)
            .reset_time_delta_seconds(reset_time_delta_seconds)
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

    #[instrument(name = "domain.agent_session.add_tool_results", skip(self, results))]
    pub async fn add_tool_results(
        &self,
        agent_id: AgentId,
        results: Vec<llm::ToolUseResult>,
    ) -> Result<llm::Prompt, AgentSessionError> {
        let mut session = self.repo.find_by_agent_id(agent_id).await?;
        let prompt = session.add_tool_results(results);
        self.repo.update(&mut session).await?;
        Ok(prompt)
    }

    #[instrument(name = "domain.agent_session.delete_for_agent_in_op", skip(self, op))]
    pub async fn delete_for_agent_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        agent_id: AgentId,
    ) -> Result<(), AgentSessionError> {
        self.repo
            .cascade_delete_for_agent_in_op(op, agent_id)
            .await?;
        Ok(())
    }

    #[instrument(name = "domain.agent_session.add_user_message", skip(self, prompt))]
    pub async fn add_user_message(
        &self,
        agent_id: AgentId,
        source: UserMessageSource,
        prompt: String,
    ) -> Result<Option<llm::Prompt>, AgentSessionError> {
        let mut session = self.repo.find_by_agent_id(agent_id).await?;
        match session.add_user_message(chrono::Utc::now(), source, prompt)? {
            es_entity::Idempotent::Executed(prompt) => {
                self.repo.update(&mut session).await?;
                Ok(Some(prompt))
            }
            es_entity::Idempotent::AlreadyApplied => Ok(None),
        }
    }
}
