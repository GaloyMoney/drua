pub mod config;
mod entity;
pub mod error;
pub mod repo;
pub mod session;

use std::sync::Arc;

use crate::audit::Audit;
use crate::skill::Skills;
use crate::toolset::ToolSets;

/// Default authorization scopes granted to an agent when it's created.
/// The `WorkspaceLead` role gets the `WorkspaceAdmin` workspace-level
/// scope (the only one for now); plain agents get nothing from this
/// function — they pick up `SandboxUseAll` / `SandboxUseReadOnly` later
/// when they're attached to a sandbox via [`Agent::sandbox_attached`].
fn default_authz_scopes(role: AgentRole, workspace_id: WorkspaceId) -> Vec<AuthScope> {
    match role {
        AgentRole::WorkspaceLead => vec![AuthScope::WorkspaceAdmin(workspace_id)],
        AgentRole::Agent => Vec::new(),
    }
}

/// Recognise a slash-skill invocation: the entire (trimmed) prompt is
/// `/<name>` with no whitespace inside. Returns the bare skill name
/// (without the leading `/`). Returns `None` for anything else,
/// including `/` alone, `/foo bar` (has args), or any prompt that
/// doesn't start with `/`.
fn parse_slash_skill(prompt: &str) -> Option<&str> {
    let trimmed = prompt.trim();
    let body = trimmed.strip_prefix('/')?;
    if body.is_empty() || body.contains(char::is_whitespace) {
        return None;
    }
    Some(body)
}

use tracing::instrument;

use crate::primitives::{AgentId, AuthScope, AuthSubject, ChatOutputEvent, SandboxId, WorkspaceId};
use crate::sandbox::{SandboxAgentMode, Sandboxes};
pub use config::{AgentsConfig, ResetTimeDeltaSeconds, RoleConfig};
pub use entity::*;
pub use error::AgentError;
use repo::AgentRepo;
use session::Sessions;

#[derive(Clone)]
pub struct Agents {
    repo: AgentRepo,
    sessions: Sessions,
    sandboxes: Arc<Sandboxes>,
    skills: Arc<Skills>,
    config: AgentsConfig,
    toolsets: Arc<ToolSets>,
    prompt_requests: llm::PromptRequestChannel,
}

impl Agents {
    pub fn new(
        pool: &sqlx::PgPool,
        config: AgentsConfig,
        toolsets: Arc<ToolSets>,
        prompt_requests: llm::PromptRequestChannel,
        sandboxes: Arc<Sandboxes>,
        skills: Arc<Skills>,
    ) -> Self {
        Self {
            repo: AgentRepo::new(pool),
            sessions: Sessions::new(pool),
            sandboxes,
            skills,
            config,
            toolsets,
            prompt_requests,
        }
    }

    pub fn skills(&self) -> &Skills {
        &self.skills
    }

    #[instrument(name = "domain.agent.create", skip(self))]
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        agent_role: AgentRole,
        name: impl Into<String> + std::fmt::Debug,
        attach_sandbox: Option<(SandboxId, SandboxAgentMode)>,
    ) -> Result<Agent, AgentError> {
        let id = AgentId::new();
        let mut op = self.repo.begin_op().await?;
        let agent = self
            .create_in_op(&mut op, id, workspace_id, agent_role, name, attach_sandbox)
            .await?;
        op.commit().await?;
        Ok(agent)
    }

    /// Composable variant of [`Self::create`]. When `attach_sandbox` is
    /// `Some((sandbox_id, mode))`, the agent is attached to the sandbox as
    /// part of the same op — the entity-level invariants (workspace match,
    /// single-writer) are enforced by
    /// [`crate::sandbox::Sandboxes::attach_to_agent_in_op`] and the
    /// matching `SandboxRead`/`SandboxWrite` scopes are written via
    /// [`Agent::sandbox_attached`]. Bypasses the subject-based authz check
    /// that [`Self::attach_sandbox`] performs — callers of `create_in_op`
    /// are trusted to have authorized the action upstream.
    #[instrument(name = "domain.agent.create_in_op", skip(self, op))]
    pub async fn create_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        id: AgentId,
        workspace_id: WorkspaceId,
        agent_role: AgentRole,
        name: impl Into<String> + std::fmt::Debug,
        attach_sandbox: Option<(SandboxId, SandboxAgentMode)>,
    ) -> Result<Agent, AgentError> {
        let role_config = self
            .config
            .builtin_roles
            .get(&agent_role)
            .ok_or(AgentError::RoleNotConfigured(agent_role))?
            .clone();

        let authz_scopes = default_authz_scopes(agent_role, workspace_id);

        let new_agent = NewAgent::builder()
            .id(id)
            .workspace_id(workspace_id)
            .agent_role(agent_role)
            .name(name)
            .authz_scopes(authz_scopes)
            .build()
            .expect("NewAgent build");

        let mut agent = self.repo.create_in_op(op, new_agent).await?;

        // Build the prompt's `tools` array from the registry as if the
        // agent were calling them — it will, with these same scopes, once
        // the session is live.
        let agent_subject = agent.auth_subject();
        let tool_defs: Vec<session::message::ToolDefinition> = self
            .toolsets
            .top_level_tools(&agent_subject)
            .map(|t| session::message::ToolDefinition::from(llm::prompt::Tool::from(t.as_ref())))
            .collect();
        let system_blocks: Vec<session::message::SystemBlock> = role_config
            .system
            .into_iter()
            .map(session::message::SystemBlock::from)
            .collect();

        self.sessions
            .create_in_op(
                op,
                agent.id,
                session::ModelSettings {
                    model: role_config.model,
                    max_tokens: role_config.max_tokens,
                },
                session::ThreadSimplificationSettings {
                    simplify_after_idle_seconds: None,
                },
                system_blocks,
                tool_defs,
            )
            .await?;

        if let Some((sandbox_id, mode)) = attach_sandbox {
            // Agent side first — `sandbox_attached` rejects a WorkspaceLead
            // role before we touch the sandbox side.
            if agent.sandbox_attached(sandbox_id, mode)?.did_execute() {
                self.repo.update_in_op(op, &mut agent).await?;
            }
            self.sandboxes
                .attach_to_agent_in_op(op, workspace_id, sandbox_id, agent.id, mode)
                .await?;
        }

        Ok(agent)
    }

    #[instrument(name = "domain.agent.find_by_id", skip(self))]
    pub async fn find_by_id(
        &self,
        id: impl Into<AgentId> + std::fmt::Debug,
    ) -> Result<Agent, AgentError> {
        Ok(self.repo.find_by_id(id.into()).await?)
    }

    #[instrument(name = "domain.agent.list_for_workspace", skip(self))]
    pub async fn list_for_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<Agent>, AgentError> {
        let query = es_entity::PaginatedQueryArgs {
            first: 100,
            after: None,
        };
        let result = self
            .repo
            .list_for_workspace_id_by_created_at(
                workspace_id,
                query,
                es_entity::ListDirection::Descending,
            )
            .await?;
        Ok(result.entities)
    }

    #[instrument(name = "domain.agent.delete_in_op", skip(self, op))]
    pub async fn delete_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        id: impl Into<AgentId> + std::fmt::Debug,
    ) -> Result<(), AgentError> {
        let id = id.into();
        let agent = self.repo.find_by_id(id).await?;
        // Cascade soft-delete to the agent's session and all its threads
        // before deleting the agent itself (mirrors workspace → agents).
        self.sessions.delete_for_agent_in_op(op, id).await?;
        self.repo.delete_in_op(op, agent).await?;
        Ok(())
    }

    /// Attach a sandbox to an agent in `mode` (Read or Write). The subject
    /// must hold [`AuthScope::WorkspaceAdmin`] on the agent's workspace.
    /// Re-attach with a different mode is allowed (downgrade unconditional;
    /// upgrade to Write succeeds only if no other agent currently holds
    /// Write — see [`crate::sandbox::Sandbox::attach_agent`]). After the
    /// entity-level attach, the matching `SandboxRead`/`SandboxWrite`
    /// scope is added to the agent (and any stale opposite-mode scope for
    /// the same sandbox is removed).
    #[instrument(name = "domain.agent.attach_sandbox", skip(self, subject))]
    pub async fn attach_sandbox(
        &self,
        subject: &AuthSubject,
        agent_id: AgentId,
        sandbox_id: SandboxId,
        mode: SandboxAgentMode,
    ) -> Result<Agent, AgentError> {
        let agent = self.repo.find_by_id(agent_id).await?;
        let workspace_id = agent.workspace_id;

        if !subject.has_any(&[AuthScope::Admin, AuthScope::WorkspaceAdmin(workspace_id)]) {
            return Err(AgentError::Unauthorized);
        }

        let mut op = self.repo.begin_op().await?;

        // Agent side first: `sandbox_attached` enforces the entity-level
        // invariants (lead can't attach; at most one sandbox per agent).
        // Failing here short-circuits before the sandbox-side round-trip.
        let mut agent = self.repo.find_by_id(agent_id).await?;
        if agent.sandbox_attached(sandbox_id, mode)?.did_execute() {
            self.repo.update_in_op(&mut op, &mut agent).await?;
        }

        // Then sandbox side (enforces workspace match and single-writer).
        self.sandboxes
            .attach_to_agent_in_op(&mut op, workspace_id, sandbox_id, agent_id, mode)
            .await?;

        op.commit().await?;
        Ok(agent)
    }

    /// Detach a sandbox from an agent. Authz mirrors `attach_sandbox`.
    /// Idempotent at both layers — entity attach list and agent scope.
    #[instrument(name = "domain.agent.detach_sandbox", skip(self, subject))]
    pub async fn detach_sandbox(
        &self,
        subject: &AuthSubject,
        agent_id: AgentId,
        sandbox_id: SandboxId,
    ) -> Result<Agent, AgentError> {
        let agent = self.repo.find_by_id(agent_id).await?;
        if !subject.has_any(&[
            AuthScope::Admin,
            AuthScope::WorkspaceAdmin(agent.workspace_id),
        ]) {
            return Err(AgentError::Unauthorized);
        }

        let mut op = self.repo.begin_op().await?;

        // Symmetric with attach: agent side first, then sandbox side.
        let mut agent = self.repo.find_by_id(agent_id).await?;
        if agent.sandbox_detached(sandbox_id).did_execute() {
            self.repo.update_in_op(&mut op, &mut agent).await?;
        }

        self.sandboxes
            .detach_from_agent_in_op(&mut op, sandbox_id, agent_id)
            .await?;

        op.commit().await?;
        Ok(agent)
    }

    #[instrument(name = "domain.agent.send_message", skip(self, prompt))]
    pub async fn send_message(
        &self,
        subject: AuthSubject,
        id: AgentId,
        prompt: String,
    ) -> Result<tokio::sync::mpsc::Receiver<ChatOutputEvent>, AgentError> {
        let agent = self.repo.find_by_id(id).await?;

        // Authorization: user and exported agents may always send. Another
        // agent may only message a peer in its own workspace (whether
        // unattributed `Agent` or `AgentOnBehalfOfUser`). Anonymous is
        // rejected.
        match &subject {
            AuthSubject::User(_) | AuthSubject::ExportedAgent(_, _, _) => {}
            AuthSubject::Agent(ws, _, _) | AuthSubject::AgentOnBehalfOfUser(_, ws, _, _)
                if *ws == agent.workspace_id => {}
            _ => return Err(AgentError::Unauthorized),
        }

        Audit::record_workspace_id(agent.workspace_id);

        let source = subject.to_message_source();
        let (tx, rx) = tokio::sync::mpsc::channel::<ChatOutputEvent>(64);

        // Attribute the agent's tool calls to the originating user when one
        // is available — direct `User`, an `ExportedAgent` token, or a peer
        // `AgentOnBehalfOfUser`. Otherwise fall back to an unattributed
        // `Agent` subject.
        let agent_subject = match subject.originating_user_id() {
            Some(user_id) => agent.auth_subject_for_user(user_id),
            None => agent.auth_subject(),
        };

        // Slash-skill expansion: when the entire prompt is `/<name>` (a
        // single token, no args), treat it as a request to invoke a
        // skill of that name. The skill's body is substituted as the
        // actual prompt sent to the LLM. Lookup goes through
        // `Skills::find_by_name`, which falls back to the agent's
        // attached sandbox's `exported_skills` when no DB-registered
        // skill matches. If no skill is found anywhere, send an Error
        // event and return early — sending the literal `/foo` to the
        // LLM is rarely what the user wanted.
        let prompt = if let Some(skill_name) = parse_slash_skill(&prompt) {
            match self
                .skills
                .find_by_name(skill_name, agent.attached_sandbox_id())
                .await?
            {
                Some(body) => body,
                None => {
                    let _ = tx
                        .send(ChatOutputEvent::Error {
                            message: format!("Unknown skill: /{skill_name}"),
                        })
                        .await;
                    return Ok(rx);
                }
            }
        } else {
            prompt
        };

        let session_response = self
            .sessions
            .add_user_input(id, session::TargetThread::Main, source, prompt.clone())
            .await?;

        match session_response {
            session::AgentSessionResponse::PromptPending { .. } => {}
            // Message queued — assistant or tools still in progress
            _ => return Ok(rx),
        }

        let _ = tx
            .send(ChatOutputEvent::UserMessage {
                source,
                text: prompt,
            })
            .await;

        let prompt_state = self
            .sessions
            .next_prompt(id, session::TargetThread::Main)
            .await?;

        let (request, response_rx) = llm::PromptRequest::new(prompt_state);
        self.prompt_requests
            .send(request)
            .await
            .map_err(|_| AgentError::PromptRequestChannelClosed)?;

        let sessions = self.sessions.clone();
        let toolsets = self.toolsets.clone();
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
                            .send(ChatOutputEvent::Error {
                                message: e.to_string(),
                            })
                            .await;
                        return;
                    }
                    Err(_) => return, // executor dropped the response channel
                };

                input_tokens += response.usage.input_tokens;
                output_tokens += response.usage.output_tokens;

                let session_response = match sessions
                    .assistant_response_received(id, response.clone())
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = tx
                            .send(ChatOutputEvent::Error {
                                message: e.to_string(),
                            })
                            .await;
                        return;
                    }
                };
                forward_response(response, &tx).await;

                let next_prompt = match session_response {
                    session::AgentSessionResponse::Done => break,
                    session::AgentSessionResponse::ToolUseRequest(tool_uses) => {
                        let tool_calls: Vec<llm::RequestToolUse> = tool_uses
                            .into_iter()
                            .map(|tu| llm::RequestToolUse {
                                id: tu.id,
                                name: tu.name,
                                input: tu.input,
                            })
                            .collect();
                        let results =
                            fan_out_tool_calls(&toolsets, &agent_subject, tool_calls, &tx).await;

                        if let Err(e) = sessions.add_tool_results(id, results).await {
                            let _ = tx
                                .send(ChatOutputEvent::Error {
                                    message: e.to_string(),
                                })
                                .await;
                            return;
                        }

                        match sessions.next_prompt(id, session::TargetThread::Main).await {
                            Ok(p) => p,
                            Err(e) => {
                                let _ = tx
                                    .send(ChatOutputEvent::Error {
                                        message: e.to_string(),
                                    })
                                    .await;
                                return;
                            }
                        }
                    }
                    session::AgentSessionResponse::PromptPending { target } => {
                        match sessions.next_prompt(id, target).await {
                            Ok(p) => p,
                            Err(e) => {
                                let _ = tx
                                    .send(ChatOutputEvent::Error {
                                        message: e.to_string(),
                                    })
                                    .await;
                                return;
                            }
                        }
                    }
                    _ => break,
                };

                let (request, rx_next) = llm::PromptRequest::new(next_prompt);
                if prompt_requests.send(request).await.is_err() {
                    let _ = tx
                        .send(ChatOutputEvent::Error {
                            message: "prompt request channel closed".to_string(),
                        })
                        .await;
                    return;
                }
                next = rx_next.await;
            }

            let _ = tx
                .send(ChatOutputEvent::AssistantDone {
                    turns: turn,
                    input_tokens,
                    output_tokens,
                    duration_ms: None,
                    cost_usd: None,
                })
                .await;
        });

        Ok(rx)
    }
}

async fn fan_out_tool_calls(
    toolsets: &Arc<ToolSets>,
    subject: &AuthSubject,
    calls: Vec<llm::RequestToolUse>,
    tx: &tokio::sync::mpsc::Sender<ChatOutputEvent>,
) -> Vec<llm::ToolUseResult> {
    let dispatches = calls.into_iter().map(|tu| {
        let toolsets = toolsets.clone();
        let subject = subject.clone();
        async move {
            let id = tu.id.clone();
            let name = tu.name.clone();
            let result = match toolsets
                .call_top_level_tool(&subject, &name, tu.input.as_object().cloned())
                .await
            {
                Ok(r) => llm::ToolUseResult {
                    tool_use_id: id,
                    content: call_result_to_text(&r),
                    is_error: r.is_error.unwrap_or(false),
                },
                Err(e) => llm::ToolUseResult {
                    tool_use_id: id,
                    content: e.to_string(),
                    is_error: true,
                },
            };
            (name, result)
        }
    });

    let outcomes = futures::future::join_all(dispatches).await;
    let mut results = Vec::with_capacity(outcomes.len());
    for (name, result) in outcomes {
        let _ = tx
            .send(ChatOutputEvent::ToolResult {
                name,
                is_error: result.is_error,
            })
            .await;
        results.push(result);
    }
    results
}

fn call_result_to_text(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn forward_response(
    response: llm::PromptResponse,
    tx: &tokio::sync::mpsc::Sender<ChatOutputEvent>,
) {
    for block in response.content {
        match block {
            llm::prompt::AssistantBlock::Text { text, .. } => {
                let _ = tx.send(ChatOutputEvent::AssistantText { text }).await;
            }
            llm::prompt::AssistantBlock::ToolUse { name, input, .. } => {
                let _ = tx
                    .send(ChatOutputEvent::ToolCall {
                        name,
                        arguments: Some(input),
                    })
                    .await;
            }
            llm::prompt::AssistantBlock::Thinking { text, .. } => {
                let _ = tx.send(ChatOutputEvent::Thinking { text }).await;
            }
        }
    }
}
