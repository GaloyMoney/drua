use sqlx::PgPool;
use tracing::instrument;
use uuid::Uuid;

use drua_git_proxy::{GitProxyDecision, GitService};

use crate::primitives::{AgentId, ProjectId};

/// Append-only writer for the `sandbox_git_proxy_attempts` table.
/// One row per smart-HTTP request, immediately after the policy decision.
/// Bytes + upstream_status are filled in by [`record_completion`] after the
/// response is fully streamed.
#[derive(Clone)]
pub struct GitProxyAuditLog {
    pool: PgPool,
}

impl GitProxyAuditLog {
    pub fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }

    #[instrument(name = "domain.git_proxy.audit.record_attempt", skip(self))]
    #[allow(clippy::too_many_arguments)]
    pub async fn record_attempt(
        &self,
        agent_id: Option<AgentId>,
        project_id: Option<ProjectId>,
        owner: &str,
        repo: &str,
        service: GitService,
        refs_requested: serde_json::Value,
        decision: GitProxyDecision,
        reject_reason: Option<&str>,
    ) -> Result<Uuid, sqlx::Error> {
        let attempt_id = Uuid::new_v4();
        sqlx::query(
            r#"
            INSERT INTO sandbox_git_proxy_attempts (
                id, agent_id, project_id, owner, repo, service,
                refs_requested, decision, reject_reason
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            "#,
        )
        .bind(attempt_id)
        .bind(agent_id.map(Uuid::from))
        .bind(project_id.map(Uuid::from))
        .bind(owner)
        .bind(repo)
        .bind(service.as_str())
        .bind(refs_requested)
        .bind(decision.as_str())
        .bind(reject_reason)
        .execute(&self.pool)
        .await?;
        Ok(attempt_id)
    }

    /// Updates byte counters + upstream HTTP status after the proxied
    /// response has been fully streamed. Called only when
    /// `record_attempt` returned `Accepted`.
    #[instrument(name = "domain.git_proxy.audit.record_completion", skip(self))]
    pub async fn record_completion(
        &self,
        attempt_id: Uuid,
        bytes_sent: i64,
        bytes_received: i64,
        upstream_status: Option<i32>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE sandbox_git_proxy_attempts
               SET bytes_sent = $2,
                   bytes_received = $3,
                   upstream_status = $4
             WHERE id = $1
            "#,
        )
        .bind(attempt_id)
        .bind(bytes_sent)
        .bind(bytes_received)
        .bind(upstream_status)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
