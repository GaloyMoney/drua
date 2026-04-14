pub mod config;
mod entity;
pub mod error;
pub mod repo;
pub mod session;

use std::sync::Arc;

use crate::toolset::ToolSets;

/// Default authorization scopes granted to an agent when it's created.
/// Eventually this will move into role-config, but for now it's hard-wired:
/// `WorkspaceLead` gets read+write on its own workspace.
fn default_authz_scopes(role: AgentRole, workspace_id: WorkspaceId) -> Vec<AuthScope> {
    match role {
        AgentRole::WorkspaceLead => vec![
            AuthScope::WorkspaceRead(workspace_id),
            AuthScope::WorkspaceWrite(workspace_id),
        ],
    }
}

use tracing::instrument;

use crate::primitives::{AgentId, AuthScope, AuthSubject, ChatOutputEvent, WorkspaceId};
pub use config::{AgentsConfig, ResetTimeDeltaSeconds, RoleConfig};
pub use entity::*;
pub use error::AgentError;
use repo::AgentRepo;
use session::Sessions;

#[derive(Clone)]
pub struct Agents {
    repo: AgentRepo,
    sessions: Sessions,
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
    ) -> Self {
        Self {
            repo: AgentRepo::new(pool),
            sessions: Sessions::new(pool),
            config,
            toolsets,
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
        let role_config = self
            .config
            .builtin_roles
            .get(&agent_role)
            .ok_or(AgentError::RoleNotConfigured(agent_role))?
            .clone();

        let authz_scopes = default_authz_scopes(agent_role, workspace_id);

        let new_agent = NewAgent::builder()
            .workspace_id(workspace_id)
            .agent_role(agent_role)
            .name(name)
            .authz_scopes(authz_scopes)
            .build()
            .expect("NewAgent build");

        let agent = self.repo.create_in_op(op, new_agent).await?;

        // Build the prompt's `tools` array from the registry as if the
        // agent were calling them — it will, with these same scopes, once
        // the session is live.
        let agent_subject = agent.auth_subject();
        let tools: Vec<llm::prompt::Tool> = self
            .toolsets
            .top_level_tools(&agent_subject)
            .map(|t| llm::prompt::Tool::from(t.as_ref()))
            .collect();

        self.sessions
            .create_in_op(
                op,
                agent.id,
                role_config.model,
                role_config.system,
                tools,
                role_config.max_tokens,
                role_config.reset_time_delta_seconds,
            )
            .await?;
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

        if let Some(prompt_state) = self
            .sessions
            .add_user_message(id, source, prompt.clone())
            .await?
        {
            let _ = tx
                .send(ChatOutputEvent::UserMessage {
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

                    let tool_calls = sessions
                        .add_prompt_response(id, response.clone())
                        .await
                        .unwrap_or_default();
                    forward_response(response, &tx).await;

                    if tool_calls.is_empty() {
                        break;
                    }

                    let results =
                        fan_out_tool_calls(&toolsets, &agent_subject, tool_calls, &tx).await;

                    let updated_prompt = match sessions.add_tool_results(id, results).await {
                        Ok(p) => p,
                        Err(e) => {
                            let _ = tx
                                .send(ChatOutputEvent::Error {
                                    message: e.to_string(),
                                })
                                .await;
                            return;
                        }
                    };

                    let (request, rx_next) = llm::PromptRequest::new(updated_prompt);
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
        }

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
