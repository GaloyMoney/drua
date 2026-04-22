pub mod config;
mod entity;
pub mod error;
pub mod repo;
pub mod session;
mod system_prompt;

use std::sync::Arc;

use crate::audit::Audit;
use crate::skill::Skills;
use crate::toolset::ToolSets;

/// Default authorization scopes granted to an agent when it's created.
/// The `WorkspaceLead` role gets the `WorkspaceAdmin` workspace-level
/// scope (the only one for now); plain agents get nothing from this
/// function — they pick up `SandboxUse` / `SandboxRead` later
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

use crate::primitives::{
    AgentId, AuthResource, AuthScope, AuthSubject, AuthVerb, ChatOutputEvent, SandboxId,
    WorkspaceId,
};
use crate::sandbox::{SandboxAgentMode, Sandboxes};
pub use config::{AgentsConfig, ModelDefaults, RoleConfig};
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

    /// Create a workspace lead agent. The workspace name is passed directly
    /// because it's known at workspace creation time.
    #[instrument(name = "domain.agent.create_workspace_lead", skip(self, sub))]
    pub async fn create_workspace_lead(
        &self,
        sub: &AuthSubject,
        workspace_id: WorkspaceId,
        name: impl Into<String> + std::fmt::Debug,
        workspace_name: &str,
    ) -> Result<Agent, AgentError> {
        sub.can(AuthVerb::Create, AuthResource::Agent(workspace_id, None))?;
        Audit::record_action_if_unset("agent.create_workspace_lead");
        Audit::record_workspace_id(workspace_id);
        let id = AgentId::new();
        Audit::record_agent_id(id);
        let mut op = self.repo.begin_op().await?;
        let agent = self
            .create_in_op(
                &mut op,
                id,
                workspace_id,
                AgentRole::WorkspaceLead,
                name,
                None,
                workspace_name,
            )
            .await?;
        op.commit().await?;
        Ok(agent)
    }

    /// Create a regular agent. The workspace name is resolved automatically
    /// from the existing lead agent in the workspace.
    #[instrument(name = "domain.agent.create_agent", skip(self, sub))]
    pub async fn create_agent(
        &self,
        sub: &AuthSubject,
        workspace_id: WorkspaceId,
        name: impl Into<String> + std::fmt::Debug,
        attach_sandbox: Option<(SandboxId, SandboxAgentMode)>,
    ) -> Result<Agent, AgentError> {
        sub.can(AuthVerb::Create, AuthResource::Agent(workspace_id, None))?;
        Audit::record_action_if_unset("agent.create_agent");
        Audit::record_workspace_id(workspace_id);
        let workspace_name = self.resolve_workspace_name(workspace_id).await?;
        let id = AgentId::new();
        Audit::record_agent_id(id);
        let mut op = self.repo.begin_op().await?;
        let agent = self
            .create_in_op(
                &mut op,
                id,
                workspace_id,
                AgentRole::Agent,
                name,
                attach_sandbox,
                &workspace_name,
            )
            .await?;
        op.commit().await?;
        Ok(agent)
    }

    /// Resolve the workspace display name from the lead agent.
    async fn resolve_workspace_name(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<String, AgentError> {
        let query = es_entity::PaginatedQueryArgs {
            first: 1,
            after: None,
        };
        let result = self
            .repo
            .list_for_workspace_id_by_created_at(
                workspace_id,
                query,
                es_entity::ListDirection::Ascending,
            )
            .await?;
        result
            .entities
            .into_iter()
            .next()
            .map(|a| a.workspace_name)
            .ok_or(AgentError::NoLeadAgent(workspace_id))
    }

    /// Composable variant of [`Self::create_workspace_lead`] — creates a
    /// lead agent inside an existing op with a pre-determined id.
    #[instrument(name = "domain.agent.create_workspace_lead_in_op", skip(self, op))]
    pub async fn create_workspace_lead_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        id: AgentId,
        workspace_id: WorkspaceId,
        name: impl Into<String> + std::fmt::Debug,
        workspace_name: &str,
    ) -> Result<Agent, AgentError> {
        Audit::record_workspace_id(workspace_id);
        Audit::record_agent_id(id);
        self.create_in_op(
            op,
            id,
            workspace_id,
            AgentRole::WorkspaceLead,
            name,
            None,
            workspace_name,
        )
        .await
    }

    /// Shared inner: creates an agent of any role with all arguments
    /// explicit. [`Self::create_workspace_lead`] and [`Self::create_agent`]
    /// delegate here after resolving the workspace name.
    #[allow(clippy::too_many_arguments)]
    #[instrument(name = "domain.agent.create_in_op", skip(self, op))]
    async fn create_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        id: AgentId,
        workspace_id: WorkspaceId,
        agent_role: AgentRole,
        name: impl Into<String> + std::fmt::Debug,
        attach_sandbox: Option<(SandboxId, SandboxAgentMode)>,
        workspace_name: &str,
    ) -> Result<Agent, AgentError> {
        let role_config = self
            .config
            .builtin_roles
            .get(&agent_role)
            .ok_or(AgentError::RoleNotConfigured(agent_role))?
            .clone();

        let model_defaults = self
            .config
            .models
            .get(&role_config.model)
            .ok_or_else(|| AgentError::ModelNotConfigured(role_config.model.clone()))?;

        let authz_scopes = default_authz_scopes(agent_role, workspace_id);

        let new_agent = NewAgent::builder()
            .id(id)
            .workspace_id(workspace_id)
            .agent_role(agent_role)
            .name(name)
            .authz_scopes(authz_scopes)
            .workspace_name(workspace_name)
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
        let system_blocks = system_prompt::system_blocks_for_role(
            agent_role,
            &self.toolsets,
            &agent_subject,
            &agent.workspace_name,
        );

        let session_model_defaults = ModelDefaults {
            model: role_config.model,
            ..model_defaults.clone()
        };

        self.sessions
            .create_in_op(
                op,
                agent.id,
                session_model_defaults,
                role_config.compaction.clone(),
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
            let sandbox = self
                .sandboxes
                .attach_to_agent_in_op(op, workspace_id, sandbox_id, agent.id, mode)
                .await?;

            self.sessions
                .sandbox_notification_in_op(
                    op,
                    agent.id,
                    sandbox.name,
                    session::message::SandboxOperation::Attach {
                        mode: format!("{mode:?}").to_lowercase(),
                        mount_path: sandbox.mount_path,
                    },
                )
                .await?;
        }

        Ok(agent)
    }

    #[instrument(name = "domain.agent.find_by_id", skip(self, sub))]
    pub async fn find_by_id(
        &self,
        sub: &AuthSubject,
        id: impl Into<AgentId> + std::fmt::Debug,
    ) -> Result<Agent, AgentError> {
        let agent = self.repo.find_by_id(id.into()).await?;
        sub.can(
            AuthVerb::Read,
            AuthResource::Agent(agent.workspace_id, Some(agent.id)),
        )?;
        Audit::record_action_if_unset("agent.find_by_id");
        Audit::record_workspace_id(agent.workspace_id);
        Audit::record_agent_id(agent.id);
        Ok(agent)
    }

    #[instrument(name = "domain.agent.list_for_workspace", skip(self, sub))]
    pub async fn list_for_workspace(
        &self,
        sub: &AuthSubject,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<Agent>, AgentError> {
        sub.can(AuthVerb::Read, AuthResource::Agent(workspace_id, None))?;
        Audit::record_action_if_unset("agent.list_for_workspace");
        Audit::record_workspace_id(workspace_id);
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
        Audit::record_workspace_id(agent.workspace_id);
        Audit::record_agent_id(id);
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
        subject.can(
            AuthVerb::Update,
            AuthResource::Agent(workspace_id, Some(agent.id)),
        )?;
        Audit::record_action_if_unset("agent.attach_sandbox");
        Audit::record_workspace_id(workspace_id);
        Audit::record_agent_id(agent_id);

        let mut op = self.repo.begin_op().await?;

        // Agent side first: `sandbox_attached` enforces the entity-level
        // invariants (lead can't attach; at most one sandbox per agent).
        // Failing here short-circuits before the sandbox-side round-trip.
        let mut agent = self.repo.find_by_id(agent_id).await?;
        if agent.sandbox_attached(sandbox_id, mode)?.did_execute() {
            self.repo.update_in_op(&mut op, &mut agent).await?;
        }

        // Sandbox side (enforces workspace match and single-writer).
        let sandbox = self
            .sandboxes
            .attach_to_agent_in_op(&mut op, workspace_id, sandbox_id, agent_id, mode)
            .await?;

        // Notify the agent's session so the LLM knows a sandbox was attached.
        self.sessions
            .sandbox_notification_in_op(
                &mut op,
                agent_id,
                sandbox.name,
                session::message::SandboxOperation::Attach {
                    mode: format!("{mode:?}").to_lowercase(),
                    mount_path: sandbox.mount_path,
                },
            )
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
        subject.can(
            AuthVerb::Update,
            AuthResource::Agent(agent.workspace_id, Some(agent.id)),
        )?;
        Audit::record_action_if_unset("agent.detach_sandbox");
        Audit::record_workspace_id(agent.workspace_id);
        Audit::record_agent_id(agent_id);

        let mut op = self.repo.begin_op().await?;

        // Symmetric with attach: agent side first, then sandbox side.
        let mut agent = self.repo.find_by_id(agent_id).await?;
        if agent.sandbox_detached(sandbox_id).did_execute() {
            self.repo.update_in_op(&mut op, &mut agent).await?;
        }

        let sandbox = self
            .sandboxes
            .detach_from_agent_in_op(&mut op, sandbox_id, agent_id)
            .await?;

        // Notify the agent's session so the LLM knows a sandbox was detached.
        self.sessions
            .sandbox_notification_in_op(
                &mut op,
                agent_id,
                sandbox.name,
                session::message::SandboxOperation::Detach,
            )
            .await?;

        op.commit().await?;

        Ok(agent)
    }

    #[instrument(name = "domain.agent.chat_history", skip(self, sub))]
    pub async fn chat_history(
        &self,
        sub: &AuthSubject,
        agent_id: AgentId,
        last_n: usize,
    ) -> Result<Vec<session::history::ChatHistoryMessage>, AgentError> {
        let agent = self.repo.find_by_id(agent_id).await?;
        sub.can(
            AuthVerb::Read,
            AuthResource::Agent(agent.workspace_id, Some(agent.id)),
        )?;
        Audit::record_action_if_unset("agent.chat_history");
        Audit::record_workspace_id(agent.workspace_id);
        Audit::record_agent_id(agent_id);
        Ok(self.sessions.chat_history(agent_id, last_n).await?)
    }

    #[instrument(name = "domain.agent.thread_infos", skip(self, sub))]
    pub async fn thread_infos(
        &self,
        sub: &AuthSubject,
        agent_id: AgentId,
    ) -> Result<Vec<session::history::SessionThreadInfo>, AgentError> {
        let agent = self.repo.find_by_id(agent_id).await?;
        sub.can(
            AuthVerb::Read,
            AuthResource::Agent(agent.workspace_id, Some(agent.id)),
        )?;
        Audit::record_action_if_unset("agent.thread_infos");
        Audit::record_workspace_id(agent.workspace_id);
        Audit::record_agent_id(agent_id);
        Ok(self.sessions.thread_infos(agent_id).await?)
    }

    #[instrument(name = "domain.agent.thread_messages", skip(self, sub))]
    pub async fn thread_messages(
        &self,
        sub: &AuthSubject,
        agent_id: AgentId,
        thread_id: session::SessionThreadId,
    ) -> Result<Vec<session::history::ThreadMessage>, AgentError> {
        let agent = self.repo.find_by_id(agent_id).await?;
        sub.can(
            AuthVerb::Read,
            AuthResource::Agent(agent.workspace_id, Some(agent.id)),
        )?;
        Audit::record_action_if_unset("agent.thread_messages");
        Audit::record_workspace_id(agent.workspace_id);
        Audit::record_agent_id(agent_id);
        Ok(self.sessions.thread_messages(agent_id, thread_id).await?)
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

        Audit::record_action_if_unset("agent.send_message");
        Audit::record_workspace_id(agent.workspace_id);
        Audit::record_agent_id(id);

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

        let model_name = prompt_state.model.clone();
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
            let mut current_model = model_name;
            loop {
                turn += 1;
                let result = match next {
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

                let (response, streamed) = match result {
                    llm::PromptResult::Stream(handle) => {
                        match consume_stream(handle, &tx).await {
                            Some(resp) => (resp, true),
                            None => return, // stream error — already sent to UI
                        }
                    }
                    llm::PromptResult::Complete(response) => (response, false),
                };

                input_tokens += response.usage.input_tokens;
                output_tokens += response.usage.output_tokens;

                // Persist the complete response to the session.
                let session_response = match sessions
                    .assistant_response_received(id, response.clone(), current_model.clone())
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

                // For the Complete path, forward full blocks to UI (streaming
                // path already forwarded deltas during consumption).
                if !streamed {
                    forward_response(response, &tx).await;
                }

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

                current_model = next_prompt.model.clone();
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

/// Drain a streaming LLM response, forwarding deltas to the UI channel and
/// accumulating the final [`llm::PromptResponse`]. Returns `None` if the
/// stream errors — the error is already sent on `tx`.
async fn consume_stream(
    handle: llm::StreamHandle,
    tx: &tokio::sync::mpsc::Sender<ChatOutputEvent>,
) -> Option<llm::PromptResponse> {
    let mut acc = llm::stream::StreamAccumulator::new();
    let mut rx = handle.rx;
    while let Some(event) = rx.recv().await {
        match event {
            Ok(delta) => {
                if let Some(chat_event) = delta_to_chat_event(&delta) {
                    let _ = tx.send(chat_event).await;
                }
                acc.process(&delta);
            }
            Err(e) => {
                let _ = tx
                    .send(ChatOutputEvent::Error {
                        message: e.to_string(),
                    })
                    .await;
                return None; // never persist partial
            }
        }
    }
    Some(acc.finish())
}

fn delta_to_chat_event(delta: &llm::stream::StreamDelta) -> Option<ChatOutputEvent> {
    use llm::stream::StreamDelta;
    match delta {
        StreamDelta::TextDelta { text } => Some(ChatOutputEvent::TextDelta { text: text.clone() }),
        StreamDelta::ThinkingDelta { text } => {
            Some(ChatOutputEvent::ThinkingDelta { text: text.clone() })
        }
        StreamDelta::ToolCallStart { name, .. } => {
            Some(ChatOutputEvent::ToolCallStart { name: name.clone() })
        }
        StreamDelta::ToolCallDelta { partial_json, .. } => {
            Some(ChatOutputEvent::ToolCallInputDelta {
                partial_json: partial_json.clone(),
            })
        }
        _ => None,
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
