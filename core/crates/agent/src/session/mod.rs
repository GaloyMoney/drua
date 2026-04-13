mod entity;
pub mod error;
pub mod repo;
mod thread;

use tracing::instrument;

pub use entity::*;
use error::AgentSessionError;
use primitives::{AgentId, UserMessageSource};
use repo::AgentSessionRepo;

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

    #[instrument(name = "domain.agent_session.create_in_op", skip(self, op))]
    pub async fn create_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        agent_id: AgentId,
    ) -> Result<AgentSession, AgentSessionError> {
        let new_session = NewAgentSession::builder()
            .agent_id(agent_id)
            .build()
            .expect("NewAgentSession build");

        Ok(self.repo.create_in_op(op, new_session).await?)
    }

    #[instrument(name = "domain.agent_session.add_user_message", skip(self, _prompt))]
    pub async fn add_user_message(
        &self,
        _agent_id: AgentId,
        _source: UserMessageSource,
        _prompt: String,
    ) -> Result<(), AgentSessionError> {
        unimplemented!()
    }
}
