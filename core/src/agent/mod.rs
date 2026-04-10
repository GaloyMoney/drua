pub mod config;
mod entity;
pub mod error;
mod harness_client;
mod light;
pub(crate) mod repo;
mod sandbox;

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::instrument;

pub use config::AgentConfig;
use entity::*;
pub use entity::{Agent, ChatConfig, SandboxConfig, SandboxState};
pub use error::*;
use repo::*;

use crate::auth::AuthContext;
use crate::chat_history::{ChatHistory, ConversationId, ConversationStatus, MessageRole};
use crate::primitives::*;
use crate::toolset::ToolSets;

pub use crate::primitives::AgentMessageEvent;

/// Cached sandbox metadata to avoid redundant K8s API calls on every message.
struct SandboxCacheEntry {
    service_fqdn: String,
    image_checked_at: Instant,
    last_healthy_at: Instant,
}

/// How long to trust the image-currency check before re-checking.
const IMAGE_CHECK_TTL: std::time::Duration = std::time::Duration::from_secs(300);
/// How long to trust the health check before re-checking.
const HEALTH_CHECK_TTL: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Clone)]
pub struct Agents {
    repo: AgentRepo,
    sandbox: Option<Arc<sandbox_client::SandboxClient>>,
    harness_client: harness_client::HarnessClient,
    light_config: config::LightRuntimeConfig,
    default_storage_class: String,
    toolsets: Arc<ToolSets>,
    chat_history: ChatHistory,
    sandbox_cache: Arc<Mutex<HashMap<AgentId, SandboxCacheEntry>>>,
}

impl Agents {
    pub async fn init(
        pool: &sqlx::PgPool,
        config: AgentConfig,
        toolsets: Arc<ToolSets>,
    ) -> Result<Self, AgentError> {
        let repo = AgentRepo::new(pool);
        let chat_history = ChatHistory::new(pool);
        let sandbox = if config.sandbox.enabled {
            let client = sandbox_client::SandboxClient::try_from_env(
                config.sandbox.namespace.clone(),
                config.sandbox.template_name.clone(),
            )
            .await?;
            Some(Arc::new(client))
        } else {
            None
        };
        let default_storage_class = config
            .sandbox
            .persistence
            .as_ref()
            .map(|p| p.storage_class.clone())
            .unwrap_or_default();
        Ok(Self {
            repo,
            sandbox,
            harness_client: harness_client::HarnessClient::new(),
            light_config: config.light,
            default_storage_class,
            toolsets,
            sandbox_cache: Arc::new(Mutex::new(HashMap::new())),
            chat_history,
        })
    }

    #[instrument(name = "domain.agent.create", skip(self))]
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        agent_type: AgentType,
        user_id: UserId,
        name: impl Into<String> + std::fmt::Debug,
    ) -> Result<Agent, AgentError> {
        let mut op = self.repo.begin_op().await?;
        let agent = self
            .create_in_op(&mut op, workspace_id, agent_type, user_id, name)
            .await?;
        op.commit().await?;
        Ok(agent)
    }

    #[instrument(name = "domain.agent.create_in_op", skip(self, op))]
    pub async fn create_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        workspace_id: WorkspaceId,
        agent_type: AgentType,
        user_id: UserId,
        name: impl Into<String> + std::fmt::Debug,
    ) -> Result<Agent, AgentError> {
        let agent_name = name.into();
        let agent_id = AgentId::new();

        let new_agent = NewAgent::builder()
            .id(agent_id)
            .workspace_id(workspace_id)
            .sandbox_config(agent_type.default_sandbox_config())
            .agent_type(agent_type)
            .name(agent_name)
            .build()
            .expect("Could not build new agent");

        let agent = self.repo.create_in_op(op, new_agent).await?;
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

    /// Update the chat and sandbox configuration for an agent.
    ///
    /// Takes effect on the next message — does not affect in-flight conversations.
    /// When sandbox resource config changes, the running sandbox is destroyed so
    /// the next message triggers re-provisioning with the new resources.
    #[instrument(name = "domain.agent.update_config", skip(self))]
    pub async fn update_config(
        &self,
        id: AgentId,
        chat_config: ChatConfig,
        sandbox_config: SandboxConfig,
    ) -> Result<Agent, AgentError> {
        let mut agent = self.repo.find_by_id(id).await?;
        let chat_changed = agent.update_chat_config(chat_config).did_execute();
        let sandbox_changed = agent.update_sandbox_config(sandbox_config).did_execute();
        if chat_changed || sandbox_changed {
            self.repo.update(&mut agent).await?;
        }
        // Destroy running sandbox so the next message re-provisions with new config
        if sandbox_changed {
            self.sandbox_cache.lock().await.remove(&id);
            self.destroy_sandbox(id).await?;
        }
        Ok(agent)
    }

    /// Send a message to an agent, dispatching to the appropriate runtime
    /// based on the agent's type.
    ///
    /// - `RuntimeKind::Light` runs an in-process agentic loop via the Anthropic API
    /// - `RuntimeKind::Sandbox` runs the agent harness inside a K8s sandbox pod
    ///
    /// Returns the conversation ID and a receiver that streams `AgentMessageEvent`s.
    /// Each event is also persisted to the chat history.
    #[instrument(name = "domain.agent.send_message", skip(self, prompt))]
    pub async fn send_message(
        &self,
        id: AgentId,
        user_id: UserId,
        prompt: String,
    ) -> Result<tokio::sync::mpsc::Receiver<AgentMessageEvent>, AgentError> {
        let agent = self.repo.find_by_id(id).await?;

        // Create a conversation record
        let conversation = self
            .chat_history
            .create_conversation(agent.id, user_id)
            .await
            .map_err(AgentError::ChatHistory)?;
        let conversation_id = conversation.id;

        // Record the user's prompt
        let _ = self
            .chat_history
            .record_message(
                conversation_id,
                MessageRole::User,
                serde_json::json!(prompt),
                serde_json::json!({}),
            )
            .await;

        let rx = match agent.agent_type.runtime_kind() {
            RuntimeKind::Light => {
                let auth = AuthContext::Agent(agent.workspace_id, agent.id, Vec::new());
                let catalog = self.toolsets.catalog().with_auth(&auth);
                light::run(
                    prompt,
                    &self.light_config,
                    &agent.chat_config,
                    agent.agent_type.system_prompt(),
                    catalog,
                )
                .await?
            }
            RuntimeKind::Sandbox => self.send_message_sandbox(agent, prompt).await?,
        };

        // Wrap receiver with persistence tap
        Ok(self.wrap_with_persistence(rx, conversation_id))
    }

    /// Destroy an agent's sandbox (best-effort). Marks the agent as having
    /// lost its sandbox and deletes the underlying K8s sandbox resource.
    #[instrument(name = "domain.agent.destroy_sandbox", skip(self))]
    pub async fn destroy_sandbox(
        &self,
        id: impl Into<AgentId> + std::fmt::Debug,
    ) -> Result<(), AgentError> {
        let client = match &self.sandbox {
            Some(c) => c,
            None => return Ok(()), // sandboxes not configured, nothing to do
        };
        let mut agent = self.repo.find_by_id(id.into()).await?;
        let sandbox_name = agent.sandbox_name();

        if agent.sandbox_lost().did_execute() {
            self.repo.update(&mut agent).await?;
        }
        if let Err(e) = client.delete_sandbox(&sandbox_name).await {
            tracing::warn!(sandbox = %sandbox_name, error = %e, "Failed to delete sandbox");
        }
        Ok(())
    }

    /// Expose chat_history for query access (e.g., from web layer).
    pub fn chat_history(&self) -> &ChatHistory {
        &self.chat_history
    }

    /// Sandbox-specific send_message path.
    ///
    /// Provisions the sandbox if needed, resolves its service FQDN, then
    /// sends the message via HTTP POST to the harness and streams SSE
    /// events back through the channel.
    ///
    /// Uses a per-agent cache to skip expensive K8s API calls (image check,
    /// FQDN resolution, health probe) on the warm path.
    async fn send_message_sandbox(
        &self,
        agent: Agent,
        prompt: String,
    ) -> Result<tokio::sync::mpsc::Receiver<AgentMessageEvent>, AgentError> {
        let base_client = self
            .sandbox
            .as_ref()
            .ok_or(AgentError::SandboxNotConfigured)?;

        let client = sandbox::configure_client(
            base_client,
            &agent.sandbox_config,
            &self.default_storage_class,
        );
        let (tx, rx) = tokio::sync::mpsc::channel::<AgentMessageEvent>(64);

        let harness = self.harness_client.clone();
        let repo = self.repo.clone();
        let session_id = Some(agent.id.to_string());
        let model = Some(agent.chat_config.model.clone());
        let max_turns = Some(agent.chat_config.max_turns);
        let disallowed_tools = agent.sandbox_config.disallowed_tools.clone();
        let sandbox_cache = Arc::clone(&self.sandbox_cache);
        let agent_id = agent.id;

        tokio::spawn(async move {
            // Check cache for a previously resolved sandbox
            let cached = {
                let cache = sandbox_cache.lock().await;
                cache.get(&agent_id).map(|e| {
                    (
                        e.service_fqdn.clone(),
                        e.image_checked_at,
                        e.last_healthy_at,
                    )
                })
            };

            let service_fqdn = if let Some((fqdn, image_checked_at, last_healthy_at)) = cached {
                // Fast path: skip image check if recently verified
                if image_checked_at.elapsed() > IMAGE_CHECK_TTL {
                    // Re-check image currency periodically
                    match sandbox::ensure_sandbox(&client, agent, &repo, &tx).await {
                        Ok(_) => {}
                        Err(e) => {
                            // Sandbox was stale — evict cache and retry
                            sandbox_cache.lock().await.remove(&agent_id);
                            tracing::warn!(error = %e, "Cached sandbox stale, evicted");
                            let _ = tx
                                .send(AgentMessageEvent::Error {
                                    message: e.to_string(),
                                })
                                .await;
                            return;
                        }
                    }
                    // Update image check timestamp
                    if let Some(entry) = sandbox_cache.lock().await.get_mut(&agent_id) {
                        entry.image_checked_at = Instant::now();
                    }
                }

                // Skip health check if recently healthy
                if last_healthy_at.elapsed() > HEALTH_CHECK_TTL {
                    if let Err(e) = harness
                        .wait_healthy(&fqdn, std::time::Duration::from_secs(30))
                        .await
                    {
                        // Health check failed — evict cache
                        sandbox_cache.lock().await.remove(&agent_id);
                        tracing::warn!(error = %e, "Cached harness unhealthy, evicted");
                        let _ = tx
                            .send(AgentMessageEvent::Error {
                                message: e.to_string(),
                            })
                            .await;
                        return;
                    }
                    if let Some(entry) = sandbox_cache.lock().await.get_mut(&agent_id) {
                        entry.last_healthy_at = Instant::now();
                    }
                }

                fqdn
            } else {
                // Cold path: full provisioning flow
                let sandbox_name = match sandbox::ensure_sandbox(&client, agent, &repo, &tx).await {
                    Ok(name) => name,
                    Err(e) => {
                        tracing::error!(error = %e, "Sandbox provisioning failed");
                        let _ = tx
                            .send(AgentMessageEvent::Error {
                                message: e.to_string(),
                            })
                            .await;
                        return;
                    }
                };

                let fqdn = match client.get_service_fqdn(&sandbox_name).await {
                    Ok(fqdn) => fqdn,
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to resolve sandbox service FQDN");
                        let _ = tx
                            .send(AgentMessageEvent::Error {
                                message: format!("Failed to resolve sandbox service: {e}"),
                            })
                            .await;
                        return;
                    }
                };

                let _ = tx
                    .send(AgentMessageEvent::Service {
                        message: "Connecting to agent…".to_string(),
                    })
                    .await;
                if let Err(e) = harness
                    .wait_healthy(&fqdn, std::time::Duration::from_secs(60))
                    .await
                {
                    tracing::error!(error = %e, "Harness health check failed");
                    let _ = tx
                        .send(AgentMessageEvent::Error {
                            message: e.to_string(),
                        })
                        .await;
                    return;
                }

                // Populate cache
                sandbox_cache.lock().await.insert(
                    agent_id,
                    SandboxCacheEntry {
                        service_fqdn: fqdn.clone(),
                        image_checked_at: Instant::now(),
                        last_healthy_at: Instant::now(),
                    },
                );

                fqdn
            };

            let input = serde_json::json!({
                "prompt": prompt,
                "session_id": session_id,
                "model": model,
                "max_turns": max_turns,
                "disallowed_tools": disallowed_tools,
            });

            if let Err(e) = harness.send_message(&service_fqdn, input, tx.clone()).await {
                tracing::error!(error = %e, "Agent message relay failed");
                // Evict cache on send failure
                sandbox_cache.lock().await.remove(&agent_id);
                let _ = tx
                    .send(AgentMessageEvent::Error {
                        message: e.to_string(),
                    })
                    .await;
            } else {
                // Update last healthy timestamp on success
                if let Some(entry) = sandbox_cache.lock().await.get_mut(&agent_id) {
                    entry.last_healthy_at = Instant::now();
                }
            }
        });

        Ok(rx)
    }

    /// Wrap an event receiver with a persistence tap that records each event
    /// to the chat history while forwarding it to the caller.
    fn wrap_with_persistence(
        &self,
        mut rx: tokio::sync::mpsc::Receiver<AgentMessageEvent>,
        conversation_id: ConversationId,
    ) -> tokio::sync::mpsc::Receiver<AgentMessageEvent> {
        let (tx, new_rx) = tokio::sync::mpsc::channel(64);
        let chat_history = self.chat_history.clone();

        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                // Service events are ephemeral — forward to SSE but don't persist
                if matches!(event, AgentMessageEvent::Service { .. }) {
                    if tx.send(event).await.is_err() {
                        break;
                    }
                    continue;
                }

                let (role, content, metadata) = event_to_message(&event);

                if let Err(e) = chat_history
                    .record_message(conversation_id, role, content, metadata)
                    .await
                {
                    tracing::warn!(error = %e, "Failed to persist chat message");
                }

                // On Done/Error, complete the conversation
                match &event {
                    AgentMessageEvent::Done {
                        turns,
                        input_tokens,
                        output_tokens,
                        ..
                    } => {
                        let total_tokens = (*input_tokens as i64) + (*output_tokens as i64);
                        let _ = chat_history
                            .complete_conversation(
                                conversation_id,
                                ConversationStatus::Completed,
                                *turns as i32,
                                total_tokens,
                            )
                            .await;
                    }
                    AgentMessageEvent::Error { .. } => {
                        let _ = chat_history
                            .complete_conversation(
                                conversation_id,
                                ConversationStatus::Failed,
                                0,
                                0,
                            )
                            .await;
                    }
                    _ => {}
                }

                // Forward to caller (SSE)
                if tx.send(event).await.is_err() {
                    break;
                }
            }
        });

        new_rx
    }
}

/// Map an `AgentMessageEvent` to a (role, content, metadata) tuple for persistence.
fn event_to_message(
    event: &AgentMessageEvent,
) -> (MessageRole, serde_json::Value, serde_json::Value) {
    match event {
        AgentMessageEvent::Text { text } => (
            MessageRole::Assistant,
            serde_json::json!(text),
            serde_json::json!({}),
        ),
        AgentMessageEvent::ToolCall { name, arguments } => (
            MessageRole::ToolCall,
            serde_json::json!(name),
            serde_json::json!({ "arguments": arguments }),
        ),
        AgentMessageEvent::ToolResult { name, is_error } => (
            MessageRole::ToolResult,
            serde_json::json!(name),
            serde_json::json!({ "is_error": is_error }),
        ),
        AgentMessageEvent::Done {
            turns,
            input_tokens,
            output_tokens,
            duration_ms,
            cost_usd,
        } => (
            MessageRole::Done,
            serde_json::json!(null),
            serde_json::json!({
                "turns": turns,
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "duration_ms": duration_ms,
                "cost_usd": cost_usd,
            }),
        ),
        AgentMessageEvent::Error { message } => (
            MessageRole::Error,
            serde_json::json!(message),
            serde_json::json!({}),
        ),
        AgentMessageEvent::Service { .. } => {
            unreachable!("service events are handled before persistence")
        }
    }
}
