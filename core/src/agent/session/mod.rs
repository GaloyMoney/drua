mod compaction;
mod entity;
pub mod error;
pub(super) mod message;
mod metadata;
pub mod repo;
mod settings;
mod thread;
mod view;

use tracing::instrument;

use crate::primitives::{AgentId, UserMessageSource};
pub use entity::*;
use error::AgentSessionError;
pub use message::TargetThread;
use message::*;
use metadata::*;
use repo::AgentSessionRepo;
pub use settings::*;

es_entity::entity_id! { AgentSessionId }

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
        skip(self, op, system_blocks, tool_defs)
    )]
    pub async fn create_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        agent_id: AgentId,
        model_settings: ModelSettings,
        compaction_config: CompactionConfig,
        system_blocks: Vec<SystemBlock>,
        tool_defs: Vec<ToolDefinition>,
    ) -> Result<AgentSession, AgentSessionError> {
        let new_session = NewAgentSession::builder()
            .agent_id(agent_id)
            .model_settings(model_settings)
            .compaction_config(compaction_config)
            .system_blocks(system_blocks)
            .tool_defs(tool_defs)
            .build()
            .expect("NewAgentSession build");

        let session = self.repo.create_in_op(op, new_session).await?;
        Ok(session)
    }

    #[instrument(name = "domain.agent_session.add_user_input", skip(self, prompt))]
    pub async fn add_user_input(
        &self,
        agent_id: AgentId,
        target: TargetThread,
        source: UserMessageSource,
        prompt: String,
    ) -> Result<AgentSessionResponse, AgentSessionError> {
        let mut session = self.repo.find_by_agent_id(agent_id).await?;
        let response = session.add_user_input(target, source, prompt)?;
        self.repo.update(&mut session).await?;
        Ok(response)
    }

    #[instrument(name = "domain.agent_session.next_prompt", skip(self))]
    pub async fn next_prompt(
        &self,
        agent_id: AgentId,
        target: TargetThread,
    ) -> Result<llm::Prompt, AgentSessionError> {
        let mut session = self.repo.find_by_agent_id(agent_id).await?;
        let prompt = session.next_prompt(target)?;
        self.repo.update(&mut session).await?;
        Ok(prompt.into())
    }

    #[instrument(
        name = "domain.agent_session.assistant_response_received",
        skip(self, response)
    )]
    pub async fn assistant_response_received(
        &self,
        agent_id: AgentId,
        response: llm::PromptResponse,
    ) -> Result<AgentSessionResponse, AgentSessionError> {
        let mut session = self.repo.find_by_agent_id(agent_id).await?;
        let thread_id = session
            .current_main_thread_id()
            .ok_or(AgentSessionError::ThreadNotFound)?;

        let content: Vec<AssistantBlock> = response
            .content
            .into_iter()
            .map(AssistantBlock::from)
            .collect();
        let stop_reason = response
            .stop_reason
            .map(StopReason::from)
            .unwrap_or(StopReason::Stop);
        let metadata = AssistantResponseMetadata::from(response.usage);

        let result =
            session.assistant_response_received(thread_id, content, stop_reason, None, metadata)?;
        self.repo.update(&mut session).await?;
        Ok(result)
    }

    #[instrument(name = "domain.agent_session.add_tool_results", skip(self, results))]
    pub async fn add_tool_results(
        &self,
        agent_id: AgentId,
        results: Vec<llm::ToolUseResult>,
    ) -> Result<AgentSessionResponse, AgentSessionError> {
        let mut session = self.repo.find_by_agent_id(agent_id).await?;
        let thread_id = session
            .current_main_thread_id()
            .ok_or(AgentSessionError::ThreadNotFound)?;

        let tool_results: Vec<ToolResultInput> =
            results.into_iter().map(ToolResultInput::from).collect();
        let result = session.add_tool_results(thread_id, tool_results)?;
        self.repo.update(&mut session).await?;
        Ok(result)
    }

    #[instrument(
        name = "domain.agent_session.sandbox_notification_in_op",
        skip(self, op)
    )]
    pub async fn sandbox_notification_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        agent_id: AgentId,
        sandbox_name: String,
        operation: message::SandboxOperation,
    ) -> Result<AgentSessionResponse, AgentSessionError> {
        let mut session = self.repo.find_by_agent_id_in_op(op, agent_id).await?;
        let response = session.add_sandbox_notification(sandbox_name, operation)?;
        self.repo.update_in_op(op, &mut session).await?;
        Ok(response)
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
}
