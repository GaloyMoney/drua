pub mod config;
mod entity;
pub mod error;
mod light;
pub(crate) mod repo;
mod sandbox;

use std::sync::Arc;

use tracing::instrument;

pub use config::AgentConfig;
use entity::*;
pub use entity::{Agent, ChatConfig, SandboxConfig, SandboxState};
pub use error::*;
use repo::*;

use crate::primitives::*;
use crate::toolset::Catalog;

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
    light_config: config::LightRuntimeConfig,
    catalog: Catalog,
}

impl Agents {
    pub async fn init(
        pool: &sqlx::PgPool,
        config: AgentConfig,
        catalog: Catalog,
    ) -> Result<Self, AgentError> {
        let repo = AgentRepo::new(pool);
        let sandbox = if config.sandbox.enabled {
            let client = sandbox_client::SandboxClient::try_from_env(
                config.sandbox.namespace.clone(),
                config.sandbox.template_name.clone(),
            )
            .await?;
            let client = if let Some(ref p) = config.sandbox.persistence {
                client.with_persistence(sandbox_client::PersistenceConfig {
                    size: p.size.clone(),
                    storage_class: p.storage_class.clone(),
                    mount_path: p.mount_path.clone(),
                })
            } else {
                client
            };
            Some(Arc::new(client))
        } else {
            None
        };
        Ok(Self {
            repo,
            sandbox,
            light_config: config.light,
            catalog,
        })
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

    /// Send a message to an agent, dispatching to the appropriate runtime
    /// based on the agent's type.
    ///
    /// - `RuntimeKind::Light` runs an in-process agentic loop via the Anthropic API
    /// - `RuntimeKind::Sandbox` runs the agent harness inside a K8s sandbox pod
    #[instrument(name = "domain.agent.send_message", skip(self, prompt))]
    pub async fn send_message(
        &self,
        id: AgentId,
        prompt: String,
    ) -> Result<tokio::sync::mpsc::Receiver<AgentMessageEvent>, AgentError> {
        let agent = self.repo.find_by_id(id).await?;

        match agent.agent_type.runtime_kind() {
            RuntimeKind::Light => {
                light::run(
                    prompt,
                    &self.light_config,
                    &agent.chat_config,
                    self.catalog.clone(),
                )
                .await
            }
            RuntimeKind::Sandbox => self.send_message_sandbox(agent, prompt).await,
        }
    }

    /// Sandbox-specific send_message path.
    async fn send_message_sandbox(
        &self,
        mut agent: Agent,
        prompt: String,
    ) -> Result<tokio::sync::mpsc::Receiver<AgentMessageEvent>, AgentError> {
        let base_client = self
            .sandbox
            .as_ref()
            .ok_or(AgentError::SandboxNotConfigured)?;

        let client = sandbox::configure_client(base_client, &agent.sandbox_config);
        let sandbox_name = sandbox::ensure_sandbox(&client, &mut agent, &self.repo).await?;

        let session_id = Some(agent.id.to_string());
        let model = agent.chat_config.model.clone();
        let max_turns = agent.chat_config.max_turns;

        let client = Arc::new(client);
        let (tx, rx) = tokio::sync::mpsc::channel::<AgentMessageEvent>(64);

        tokio::spawn(async move {
            if let Err(e) = sandbox::relay_agent_message(
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
}
