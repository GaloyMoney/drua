mod entity;
pub mod error;
pub(crate) mod repo;

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::instrument;

use entity::*;
pub use entity::{Agent, ChatConfig, SandboxConfig, SandboxState};
pub use error::*;
use repo::*;

use crate::primitives::*;

/// An event emitted during an agent message exchange.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentMessageEvent {
    /// Agent harness produced a line of output.
    Data { event_type: String, data: String },
    /// An error occurred.
    Error { message: String },
}

#[derive(Clone)]
pub struct Agents {
    repo: AgentRepo,
    sandbox: Option<Arc<sandbox_client::SandboxClient>>,
}

impl Agents {
    pub fn new(pool: &sqlx::PgPool, sandbox: Option<Arc<sandbox_client::SandboxClient>>) -> Self {
        let repo = AgentRepo::new(pool);
        Self { repo, sandbox }
    }

    #[instrument(name = "domain.agent.create", skip(self))]
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        agent_type: AgentType,
        name: impl Into<String> + std::fmt::Debug,
    ) -> Result<Agent, AgentError> {
        let mut op = self.repo.begin_op().await?;
        let agent = self
            .create_in_op(&mut op, workspace_id, agent_type, name)
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
        name: impl Into<String> + std::fmt::Debug,
    ) -> Result<Agent, AgentError> {
        let new_agent = NewAgent::builder()
            .workspace_id(workspace_id)
            .agent_type(agent_type)
            .name(name)
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

    /// Send a message to an agent, ensuring its sandbox is running.
    /// Returns a channel receiver that streams agent harness events.
    #[instrument(name = "domain.agent.send_message", skip(self, prompt))]
    pub async fn send_message(
        &self,
        id: AgentId,
        prompt: String,
        session_id: Option<String>,
        model: Option<String>,
        max_turns: Option<u32>,
    ) -> Result<tokio::sync::mpsc::Receiver<AgentMessageEvent>, AgentError> {
        let base_client = self
            .sandbox
            .as_ref()
            .ok_or(AgentError::SandboxNotConfigured)?;

        let mut agent = self.repo.find_by_id(id).await?;

        // Configure sandbox client from agent's sandbox config
        let client = self.configure_client(base_client, &agent.sandbox_config);
        let sandbox_name = self.ensure_sandbox(&client, &mut agent).await?;

        // Apply agent chat config defaults for model/max_turns
        let model = model.or_else(|| agent.chat_config.model.clone());
        let max_turns = max_turns.or(agent.chat_config.max_turns);

        let client = Arc::new(client);
        let (tx, rx) = tokio::sync::mpsc::channel::<AgentMessageEvent>(64);

        tokio::spawn(async move {
            if let Err(e) = relay_agent_message(
                client,
                &sandbox_name,
                prompt,
                session_id,
                model,
                max_turns,
                tx.clone(),
            )
            .await
            {
                tracing::error!(error = %e, sandbox = %sandbox_name, "Agent message relay failed");
                let _ = tx
                    .send(AgentMessageEvent::Error {
                        message: e.to_string(),
                    })
                    .await;
            }
        });

        Ok(rx)
    }

    /// Clone the base sandbox client and apply agent-specific configuration.
    fn configure_client(
        &self,
        base: &sandbox_client::SandboxClient,
        config: &SandboxConfig,
    ) -> sandbox_client::SandboxClient {
        let client = base.clone();
        if config.persistent_volume {
            client.with_persistence(sandbox_client::PersistenceConfig {
                size: config.pvc_size.clone(),
                storage_class: String::new(),
                mount_path: "/workspace".to_string(),
            })
        } else {
            client
        }
    }

    /// Ensure the agent has a running sandbox, creating one if needed.
    async fn ensure_sandbox(
        &self,
        client: &sandbox_client::SandboxClient,
        agent: &mut Agent,
    ) -> Result<String, AgentError> {
        let sandbox_name = agent.sandbox_name();

        match agent.sandbox_state {
            SandboxState::Ready => return Ok(sandbox_name),
            SandboxState::Provisioning => {
                client
                    .wait_sandbox_ready(&sandbox_name, std::time::Duration::from_secs(120))
                    .await?;
                agent.sandbox_ready();
                self.repo.update(agent).await?;
                return Ok(sandbox_name);
            }
            SandboxState::None => {}
        }

        // Try to create; if already exists, just wait for ready
        match client.create_sandbox(&sandbox_name).await {
            Ok(_) => {}
            Err(sandbox_client::SandboxError::Kube(e))
                if e.to_string().contains("already exists") =>
            {
                tracing::info!(sandbox = %sandbox_name, "Sandbox already exists, waiting for ready");
            }
            Err(e) => return Err(e.into()),
        }

        agent.sandbox_provisioned();
        self.repo.update(agent).await?;

        client
            .wait_sandbox_ready(&sandbox_name, std::time::Duration::from_secs(120))
            .await?;

        agent.sandbox_ready();
        self.repo.update(agent).await?;

        Ok(sandbox_name)
    }
}

/// Execute the agent harness inside a sandbox pod and relay stdout → channel.
async fn relay_agent_message(
    client: Arc<sandbox_client::SandboxClient>,
    sandbox_name: &str,
    prompt: String,
    session_id: Option<String>,
    model: Option<String>,
    max_turns: Option<u32>,
    tx: tokio::sync::mpsc::Sender<AgentMessageEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let command = vec!["agent-harness".to_string()];
    let mut process = client.exec_sandbox_raw(sandbox_name, command).await?;

    let input_line = serde_json::json!({
        "prompt": prompt,
        "session_id": session_id,
        "model": model,
        "max_turns": max_turns,
    });

    let mut stdin = process
        .stdin()
        .ok_or("no stdin stream from agent harness")?;
    let payload = format!("{}\n", input_line);
    stdin.write_all(payload.as_bytes()).await?;
    drop(stdin);

    let mut stdout = process
        .stdout()
        .ok_or("no stdout stream from agent harness")?;

    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];

    loop {
        match stdout.read(&mut tmp).await {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);

                while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=pos).collect();
                    let line_str = String::from_utf8_lossy(&line).trim().to_string();
                    if line_str.is_empty() {
                        continue;
                    }

                    let event_type = serde_json::from_str::<serde_json::Value>(&line_str)
                        .ok()
                        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(String::from))
                        .unwrap_or_else(|| "message".to_string());

                    if tx
                        .send(AgentMessageEvent::Data {
                            event_type,
                            data: line_str,
                        })
                        .await
                        .is_err()
                    {
                        return Ok(());
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Agent harness stdout error");
                break;
            }
        }
    }

    // Flush remaining partial line
    if !buf.is_empty() {
        let line_str = String::from_utf8_lossy(&buf).trim().to_string();
        if !line_str.is_empty() {
            let _ = tx
                .send(AgentMessageEvent::Data {
                    event_type: "message".to_string(),
                    data: line_str,
                })
                .await;
        }
    }

    Ok(())
}
