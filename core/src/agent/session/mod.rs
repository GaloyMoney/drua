mod compaction;
mod entity;
pub mod error;
pub mod export;
pub mod history;
pub mod message;
pub mod metadata;
pub mod repo;
mod settings;
mod thread;
mod view;

use tracing::instrument;

use crate::agent::config::ModelDefaults;
use crate::primitives::{AgentId, UserMessageSource};
pub use entity::*;
use error::AgentSessionError;
pub use message::TargetThread;
use message::*;
use metadata::*;
use repo::AgentSessionRepo;
pub use settings::*;
pub use thread::SessionThreadId;

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
        model_defaults: ModelDefaults,
        compaction_config: CompactionConfig,
        system_blocks: Vec<SystemBlock>,
        tool_defs: Vec<ToolDefinition>,
    ) -> Result<AgentSession, AgentSessionError> {
        let new_session = NewAgentSession::builder()
            .agent_id(agent_id)
            .model_defaults(model_defaults)
            .compaction_config(compaction_config)
            .system_blocks(system_blocks)
            .tool_defs(tool_defs)
            .build()
            .expect("NewAgentSession build");

        let session = self.repo.create_in_op(op, new_session).await?;
        Ok(session)
    }

    /// Apply a fresh set of proposed system blocks (notes/skills/etc) and
    /// record a user input in a single DB round-trip. The `proposed_system_blocks`
    /// vec carries the caller's current view of the workspace context — any
    /// kind that differs from what's persisted is recorded as a
    /// `SystemBlockUpdated` event. Thread refresh happens lazily in
    /// `next_prompt`.
    #[instrument(
        name = "domain.agent_session.add_user_input",
        skip(self, prompt, proposed_system_blocks)
    )]
    pub async fn add_user_input(
        &self,
        agent_id: AgentId,
        target: TargetThread,
        source: UserMessageSource,
        prompt: String,
        proposed_system_blocks: Vec<SystemBlock>,
    ) -> Result<AgentSessionResponse, AgentSessionError> {
        let mut session = self.repo.find_by_agent_id(agent_id).await?;
        let _ = session.apply_proposed_system_blocks(proposed_system_blocks);
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
        model: String,
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
        let mut metadata = AssistantResponseMetadata::from(response.usage);
        metadata.model = model;

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

    #[instrument(name = "domain.agent_session.chat_history", skip(self))]
    pub async fn chat_history(
        &self,
        agent_id: AgentId,
        last_n: usize,
    ) -> Result<Vec<history::ChatHistoryMessage>, AgentSessionError> {
        let session = self.repo.find_by_agent_id(agent_id).await?;
        Ok(session.chat_history(last_n))
    }

    #[instrument(name = "domain.agent_session.thread_infos", skip(self))]
    pub async fn thread_infos(
        &self,
        agent_id: AgentId,
    ) -> Result<Vec<history::SessionThreadInfo>, AgentSessionError> {
        let session = self.repo.find_by_agent_id(agent_id).await?;
        Ok(session.thread_infos())
    }

    #[instrument(name = "domain.agent_session.thread_messages", skip(self))]
    pub async fn thread_messages(
        &self,
        agent_id: AgentId,
        thread_id: SessionThreadId,
    ) -> Result<Vec<history::ThreadMessage>, AgentSessionError> {
        let session = self.repo.find_by_agent_id(agent_id).await?;
        session.thread_messages(thread_id)
    }

    #[instrument(name = "domain.agent_session.thread_system_view", skip(self))]
    pub async fn thread_system_view(
        &self,
        agent_id: AgentId,
        thread_id: SessionThreadId,
    ) -> Result<history::ThreadSystemView, AgentSessionError> {
        let session = self.repo.find_by_agent_id(agent_id).await?;
        session.thread_system_view(thread_id)
    }

    #[instrument(name = "domain.agent_session.current_thread_id", skip(self))]
    pub async fn current_thread_id(
        &self,
        agent_id: AgentId,
    ) -> Result<Option<SessionThreadId>, AgentSessionError> {
        let session = self.repo.find_by_agent_id(agent_id).await?;
        Ok(session.current_main_thread_id())
    }

    #[instrument(name = "domain.agent_session.export_thread", skip(self))]
    pub async fn export_thread(
        &self,
        agent_id: AgentId,
        target: TargetThread,
    ) -> Result<export::ExportableThread, AgentSessionError> {
        let session = self.repo.find_by_agent_id(agent_id).await?;
        session.exportable_thread(target)
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
