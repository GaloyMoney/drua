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

use crate::{
    agent::{config::AgentsConfig, AgentRole},
    primitives::{AgentId, UserMessageSource},
};
pub use entity::*;
use error::AgentSessionError;
pub use message::TargetThread;
use message::*;
use metadata::*;
use repo::AgentSessionRepo;
pub use settings::*;
pub use thread::SessionThreadId;

es_entity::entity_id! { AgentSessionId }

/// Synthetic tool-result content injected when a workflow step is retried at
/// the job level (e.g. a pod restart) while a tool call was still in flight.
/// The previous execution died before recording the result, so the reused
/// agent picks the conversation back up with this note in place of the lost
/// result.
const TOOL_INTERRUPTED_ON_RETRY: &str =
    "tool_execution_interrupted: the previous execution of this \
    step was interrupted before this tool call returned (e.g. the worker restarted). Its result is \
    lost and any side effects are unconfirmed. Re-verify the relevant state before relying on it, \
    then continue.";

#[derive(Clone)]
pub struct Sessions {
    repo: AgentSessionRepo,
    config: AgentsConfig,
}

impl Sessions {
    pub fn new(pool: &sqlx::PgPool, config: AgentsConfig) -> Self {
        Self {
            repo: AgentSessionRepo::new(pool),
            config,
        }
    }

    #[instrument(name = "domain.agent_session.update_chain_for_agent", skip(self))]
    pub async fn update_chain_for_agent(
        &self,
        agent_role: AgentRole,
        agent_id: AgentId,
        chain_override: Option<llm::ModelChain>,
    ) -> Result<AgentSession, AgentSessionError> {
        let resolved = self
            .config
            .resolve_chain(agent_role, chain_override.clone())
            .map_err(|e| AgentSessionError::ModelChainInvalid(e.to_string()))?;

        let mut op = self.repo.begin_op().await?;
        let mut session = self.repo.find_by_agent_id_in_op(&mut op, agent_id).await?;
        if session
            .update_model_chain(chain_override, resolved)
            .did_execute()
        {
            self.repo.update_in_op(&mut op, &mut session).await?;
        }
        op.commit().await?;
        Ok(session)
    }

    #[allow(clippy::too_many_arguments)]
    #[instrument(
        name = "domain.agent_session.create_in_op",
        skip(self, op, system_blocks, tool_defs)
    )]
    pub async fn create_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        agent_id: AgentId,
        agent_role: AgentRole,
        chain_override: Option<llm::ModelChain>,
        compaction_config: CompactionConfig,
        system_blocks: Vec<SystemBlock>,
        tool_defs: Vec<ToolDefinition>,
    ) -> Result<AgentSession, AgentSessionError> {
        let model_chain = self
            .config
            .resolve_chain(agent_role, chain_override.clone())
            .map_err(|e| AgentSessionError::ModelChainInvalid(e.to_string()))?;
        let new_session = NewAgentSession::builder()
            .agent_id(agent_id)
            .agent_role(agent_role)
            .chain_override(chain_override)
            .model_chain(model_chain)
            .compaction_config(compaction_config)
            .system_blocks(system_blocks)
            .tool_defs(tool_defs)
            .build()
            .expect("NewAgentSession build");

        let session = self.repo.create_in_op(op, new_session).await?;
        Ok(session)
    }

    /// Diffs proposed blocks against persisted; differing kinds emit
    /// `SystemBlockUpdated`. Thread refresh happens lazily in `next_prompt`.
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
        let mut op = self.repo.begin_op().await?;
        let mut session = self.repo.find_by_agent_id_in_op(&mut op, agent_id).await?;
        let _ = session.apply_proposed_system_blocks(proposed_system_blocks);
        let response = session.add_user_input(target, source, prompt)?;
        self.repo.update_in_op(&mut op, &mut session).await?;
        op.commit().await?;
        Ok(response)
    }

    #[instrument(name = "domain.agent_session.next_prompt", skip(self))]
    pub async fn next_prompt(
        &self,
        agent_id: AgentId,
        target: TargetThread,
    ) -> Result<llm::Prompt, AgentSessionError> {
        let mut op = self.repo.begin_op().await?;
        let mut session = self.repo.find_by_agent_id_in_op(&mut op, agent_id).await?;

        // Drift-propagate config changes to inherited sessions; workflow agents are immutable for the run.
        if session.chain_override.is_none() && !session.is_workflow_agent() {
            let resolved = self
                .config
                .resolve_chain(session.agent_role, None)
                .map_err(|e| AgentSessionError::ModelChainInvalid(e.to_string()))?;
            let _ = session.update_model_chain(None, resolved);
        }

        let prompt = session.next_prompt(target)?;
        self.repo.update_in_op(&mut op, &mut session).await?;
        op.commit().await?;
        Ok(prompt.into())
    }

    /// Rebuilds the prompt of the most recent unanswered `PromptSent`.
    /// Resume primitive: workflow runner re-drives this prompt after a kill
    /// instead of re-invoking `add_user_input` + `next_prompt` (which would
    /// duplicate the user message and re-run compaction).
    #[instrument(name = "domain.agent_session.pending_prompt", skip(self))]
    pub async fn pending_prompt(
        &self,
        agent_id: AgentId,
    ) -> Result<Option<llm::Prompt>, AgentSessionError> {
        let session = self.repo.find_by_agent_id(agent_id).await?;
        Ok(session.pending_prompt()?.map(Into::into))
    }

    /// Resume-after-restart primitive for the workflow runner. When the prior
    /// execution died *after* the assistant requested tools but *before* their
    /// results were recorded (a pod restart mid-tool-call), the turn is stuck
    /// in a ToolUse state that [`Self::pending_prompt`] cannot rebuild. Close
    /// the unanswered tool_use with a synthetic interrupted-result and return
    /// the continuation prompt so the same agent picks up where it left off,
    /// regardless of which tool was in flight. `Ok(None)` when the session is
    /// not stuck in a ToolUse turn (caller falls back to a fresh turn).
    #[instrument(name = "domain.agent_session.resume_interrupted_tool_use", skip(self))]
    pub async fn resume_interrupted_tool_use(
        &self,
        agent_id: AgentId,
    ) -> Result<Option<llm::Prompt>, AgentSessionError> {
        let mut op = self.repo.begin_op().await?;
        let mut session = self.repo.find_by_agent_id_in_op(&mut op, agent_id).await?;
        if !session
            .abort_pending_tool_use(TOOL_INTERRUPTED_ON_RETRY)?
            .did_execute()
        {
            return Ok(None);
        }
        let prompt = session.next_prompt(TargetThread::Main)?;
        self.repo.update_in_op(&mut op, &mut session).await?;
        op.commit().await?;
        Ok(Some(prompt.into()))
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
        let mut op = self.repo.begin_op().await?;
        let mut session = self.repo.find_by_agent_id_in_op(&mut op, agent_id).await?;
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
        self.repo.update_in_op(&mut op, &mut session).await?;
        op.commit().await?;
        Ok(result)
    }

    /// Records a failed assistant turn. Without this the thread stays in
    /// `AwaitingAssistantResponse` after an LLM error and every subsequent
    /// user message bounces with "session is waiting for an assistant
    /// response."
    #[instrument(name = "domain.agent_session.assistant_response_failed", skip(self))]
    pub async fn assistant_response_failed(
        &self,
        agent_id: AgentId,
        model: String,
        error_message: String,
    ) -> Result<AgentSessionResponse, AgentSessionError> {
        let mut op = self.repo.begin_op().await?;
        let mut session = self.repo.find_by_agent_id_in_op(&mut op, agent_id).await?;
        let thread_id = session
            .current_main_thread_id()
            .ok_or(AgentSessionError::ThreadNotFound)?;

        let metadata = AssistantResponseMetadata {
            api: String::new(),
            model,
            usage: Usage {
                input: 0,
                output: 0,
                cache_read: 0,
                cache_write: 0,
                total_tokens: 0,
                reasoning: 0,
            },
            cost: Cost::default(),
        };

        let result = session.assistant_response_received(
            thread_id,
            Vec::new(),
            StopReason::Error,
            Some(error_message),
            metadata,
        )?;
        self.repo.update_in_op(&mut op, &mut session).await?;
        op.commit().await?;
        Ok(result)
    }

    #[instrument(name = "domain.agent_session.add_tool_results", skip(self, results))]
    pub async fn add_tool_results(
        &self,
        agent_id: AgentId,
        results: Vec<llm::ToolUseResult>,
    ) -> Result<AgentSessionResponse, AgentSessionError> {
        let mut op = self.repo.begin_op().await?;
        let mut session = self.repo.find_by_agent_id_in_op(&mut op, agent_id).await?;
        let thread_id = session
            .current_main_thread_id()
            .ok_or(AgentSessionError::ThreadNotFound)?;

        let tool_results: Vec<ToolResultInput> =
            results.into_iter().map(ToolResultInput::from).collect();
        let result = session.add_tool_results(thread_id, tool_results)?;
        self.repo.update_in_op(&mut op, &mut session).await?;
        op.commit().await?;
        Ok(result)
    }

    /// Records a sandbox attach/detach in chat history. `workspace_text` is
    /// the pre-wrapped `Workspace` system block content; on Attach with
    /// `Some` it's pushed alongside the notification, on Detach the Workspace
    /// block is cleared automatically. Bundling these here means callers can't
    /// forget to keep CLAUDE.md in sync with the attached sandbox.
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
        workspace_text: Option<String>,
    ) -> Result<AgentSessionResponse, AgentSessionError> {
        let mut session = self.repo.find_by_agent_id_in_op(op, agent_id).await?;
        let response = session.add_sandbox_notification(sandbox_name, operation, workspace_text)?;
        self.repo.update_in_op(op, &mut session).await?;
        Ok(response)
    }

    /// Pushes a `SystemBlock` to the agent's session in `op`. Idempotent —
    /// no event is emitted when `block`'s content equals the latest persisted
    /// system block.
    #[instrument(name = "domain.agent_session.push_system_block_in_op", skip(self, op))]
    pub async fn push_system_block_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        agent_id: AgentId,
        block: message::SystemBlock,
    ) -> Result<es_entity::Idempotent<()>, AgentSessionError> {
        let mut session = self.repo.find_by_agent_id_in_op(op, agent_id).await?;
        let outcome = session.push_system_block(block);
        if outcome.did_execute() {
            self.repo.update_in_op(op, &mut session).await?;
        }
        Ok(outcome)
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

    #[instrument(name = "domain.agent_session.find_for_agent", skip(self))]
    pub async fn find_for_agent(
        &self,
        agent_id: AgentId,
    ) -> Result<AgentSession, AgentSessionError> {
        Ok(self.repo.find_by_agent_id(agent_id).await?)
    }

    /// Reads the latest persisted `OutputSubmitted` value for an
    /// agent's session. `None` when no submit_output call has been
    /// recorded. Workflow executor consumes this to populate
    /// `StepResult.output`.
    #[instrument(name = "domain.agent_session.submitted_output", skip(self))]
    pub async fn submitted_output(
        &self,
        agent_id: AgentId,
    ) -> Result<Option<serde_json::Value>, AgentSessionError> {
        let session = self.repo.find_by_agent_id(agent_id).await?;
        Ok(session.submitted_output().cloned())
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

    /// Used by the agent-delete cascade. The optional `detach` records
    /// a sandbox-detach notification on the session (mirrors
    /// [`Self::sandbox_notification_in_op`]) right before the soft
    /// delete, so the chat history reflects the detach without an extra
    /// load. `EsRepo::delete_in_op` cascades to nested `session_threads`.
    #[instrument(name = "domain.agent_session.delete_for_agent_in_op", skip(self, op))]
    pub async fn delete_for_agent_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        agent_id: AgentId,
        detach: Option<(String, message::SandboxOperation)>,
    ) -> Result<(), AgentSessionError> {
        let mut session = self.repo.find_by_agent_id_in_op(op, agent_id).await?;
        if let Some((sandbox_name, operation)) = detach {
            // Session is about to be deleted; the workspace-clear event the
            // detach emits is moot, but we still go through the same path to
            // keep the chat-history projection consistent.
            session.add_sandbox_notification(sandbox_name, operation, None)?;
        }
        self.repo.delete_in_op(op, session).await?;
        Ok(())
    }
}
