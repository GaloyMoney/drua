pub mod config;
mod entity;
pub mod error;
mod harness_pool;
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

use crate::auth::AuthContext;
use crate::mcp_creds::McpCredentials;
use crate::primitives::*;
use crate::toolset::ToolSets;

pub use crate::primitives::AgentMessageEvent;

#[derive(Clone)]
pub struct Agents {
    repo: AgentRepo,
    sandbox: Option<Arc<sandbox_client::SandboxClient>>,
    harness_pool: harness_pool::HarnessPool,
    light_config: config::LightRuntimeConfig,
    toolsets: Arc<ToolSets>,
    mcp_creds: McpCredentials,
}

impl Agents {
    pub async fn init(
        pool: &sqlx::PgPool,
        config: AgentConfig,
        toolsets: Arc<ToolSets>,
        mcp_creds: McpCredentials,
    ) -> Result<Self, AgentError> {
        let repo = AgentRepo::new(pool);
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
        Ok(Self {
            repo,
            sandbox,
            harness_pool: harness_pool::HarnessPool::new(),
            light_config: config.light,
            toolsets,
            mcp_creds,
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
        let (_, token_hash) = crate::mcp_creds::token::generate_token();
        let creds = self
            .mcp_creds
            .create_in_op(
                op,
                McpCredsOwner::Agent { agent_id },
                format!("agent:{agent_name}"),
                token_hash,
                vec!["agent".to_string()],
            )
            .await?;

        let new_agent = NewAgent::builder()
            .id(agent_id)
            .workspace_id(workspace_id)
            .sandbox_config(agent_type.default_sandbox_config())
            .agent_type(agent_type)
            .name(agent_name)
            .mcp_creds_id(creds.id)
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
        user_id: UserId,
        prompt: String,
    ) -> Result<tokio::sync::mpsc::Receiver<AgentMessageEvent>, AgentError> {
        let agent = self.repo.find_by_id(id).await?;

        match agent.agent_type.runtime_kind() {
            RuntimeKind::Light => {
                let auth = AuthContext::InternalAgent(user_id, agent.id, agent.mcp_creds_id);
                let catalog = self.toolsets.catalog().with_auth(&auth);
                light::run(
                    prompt,
                    &self.light_config,
                    &agent.chat_config,
                    agent.agent_type.system_prompt(),
                    catalog,
                )
                .await
            }
            RuntimeKind::Sandbox => self.send_message_sandbox(agent, prompt).await,
        }
    }

    /// Sandbox-specific send_message path.
    ///
    /// Uses the [`HarnessPool`](harness_pool::HarnessPool) to reuse an
    /// existing exec session when possible, avoiding the 1-3 s harness
    /// startup cost on subsequent messages.
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
        let model = Some(agent.chat_config.model.clone());
        let max_turns = Some(agent.chat_config.max_turns);

        let client = Arc::new(client);
        let (tx, rx) = tokio::sync::mpsc::channel::<AgentMessageEvent>(64);

        let pool = self.harness_pool.clone();
        let msg = harness_pool::HarnessMessage {
            agent_id: agent.id,
            sandbox_name: sandbox_name.clone(),
            client,
            prompt,
            session_id,
            model,
            max_turns,
        };

        tokio::spawn(async move {
            if let Err(e) = pool.send_message(msg, tx.clone()).await {
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
