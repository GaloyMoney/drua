//! DB ownership registry for tunnel WebSockets.
//!
//! Mutations notify in the same transaction so peers cannot miss a
//! committed ownership change because `pg_notify` failed afterward.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};

use crate::protocol::RegisteredToolSet;

const NOTIFY_CHANNEL: &str = "tunnel_registry_changed";

#[derive(Debug, Clone)]
pub struct TunnelRegistrationRow {
    pub deployment_id: String,
    pub session_id: uuid::Uuid,
    pub owner_pod_addr: String,
    pub toolsets: Vec<RegisteredToolSet>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ToolsetsPayload(Vec<RegisteredToolSet>);

type ActiveRow = (
    String,
    uuid::Uuid,
    String,
    sqlx::types::Json<ToolsetsPayload>,
    DateTime<Utc>,
);

#[derive(Clone)]
pub struct TunnelRegistrations {
    pool: PgPool,
}

impl TunnelRegistrations {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn upsert(
        &self,
        deployment_id: &str,
        session_id: uuid::Uuid,
        owner_pod_addr: &str,
        toolsets: &[RegisteredToolSet],
        ttl: Duration,
    ) -> Result<(), sqlx::Error> {
        let payload = sqlx::types::Json(ToolsetsPayload(toolsets.to_vec()));
        let ttl_secs = ttl.as_secs_f64();
        let mut tx = self.pool.begin().await?;
        sqlx::query!(
            r#"
            INSERT INTO tunnel_registrations
                (deployment_id, session_id, owner_pod_addr, toolsets, expires_at)
            VALUES
                ($1, $2, $3, $4, now() + ($5::double precision * INTERVAL '1 second'))
            ON CONFLICT (deployment_id) DO UPDATE
                SET session_id     = EXCLUDED.session_id,
                    owner_pod_addr = EXCLUDED.owner_pod_addr,
                    toolsets       = EXCLUDED.toolsets,
                    registered_at  = now(),
                    expires_at     = EXCLUDED.expires_at
            "#,
            deployment_id,
            session_id,
            owner_pod_addr,
            payload as _,
            ttl_secs,
        )
        .execute(&mut *tx)
        .await?;

        Self::notify_in_tx(&mut tx, deployment_id).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn heartbeat(
        &self,
        deployment_id: &str,
        session_id: uuid::Uuid,
        ttl: Duration,
    ) -> Result<bool, sqlx::Error> {
        let ttl_secs = ttl.as_secs_f64();
        let result = sqlx::query!(
            r#"
            UPDATE tunnel_registrations
               SET expires_at = now() + ($3::double precision * INTERVAL '1 second')
             WHERE deployment_id = $1 AND session_id = $2 AND expires_at > now()
            "#,
            deployment_id,
            session_id,
            ttl_secs,
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn delete_if_owner(
        &self,
        deployment_id: &str,
        session_id: uuid::Uuid,
    ) -> Result<u64, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(
            r#"
            DELETE FROM tunnel_registrations
             WHERE deployment_id = $1 AND session_id = $2
            "#,
        )
        .bind(deployment_id)
        .bind(session_id)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() > 0 {
            Self::notify_in_tx(&mut tx, deployment_id).await?;
        }
        tx.commit().await?;
        Ok(result.rows_affected())
    }

    pub async fn delete_all_owned_by(&self, owner_pod_addr: &str) -> Result<u64, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let rows: Vec<(String,)> = sqlx::query_as(
            r#"
            DELETE FROM tunnel_registrations
             WHERE owner_pod_addr = $1
             RETURNING deployment_id
            "#,
        )
        .bind(owner_pod_addr)
        .fetch_all(&mut *tx)
        .await?;
        for (deployment_id,) in &rows {
            Self::notify_in_tx(&mut tx, deployment_id).await?;
        }
        tx.commit().await?;
        Ok(rows.len() as u64)
    }

    pub async fn reap_expired(&self) -> Result<u64, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let rows: Vec<(String,)> = sqlx::query_as(
            r#"
            DELETE FROM tunnel_registrations
             WHERE expires_at < now()
             RETURNING deployment_id
            "#,
        )
        .fetch_all(&mut *tx)
        .await?;
        for (deployment_id,) in &rows {
            Self::notify_in_tx(&mut tx, deployment_id).await?;
        }
        tx.commit().await?;
        Ok(rows.len() as u64)
    }

    pub async fn fetch_active(
        &self,
        deployment_id: &str,
    ) -> Result<Option<TunnelRegistrationRow>, sqlx::Error> {
        let row: Option<(
            uuid::Uuid,
            String,
            sqlx::types::Json<ToolsetsPayload>,
            DateTime<Utc>,
        )> = sqlx::query_as(
            r#"
            SELECT session_id, owner_pod_addr, toolsets, expires_at
              FROM tunnel_registrations
             WHERE deployment_id = $1 AND expires_at > now()
            "#,
        )
        .bind(deployment_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(
            |(session_id, owner_pod_addr, toolsets, expires_at)| TunnelRegistrationRow {
                deployment_id: deployment_id.to_string(),
                session_id,
                owner_pod_addr,
                toolsets: toolsets.0 .0,
                expires_at,
            },
        ))
    }

    pub async fn fetch_all_active(&self) -> Result<Vec<TunnelRegistrationRow>, sqlx::Error> {
        let rows: Vec<ActiveRow> = sqlx::query_as(
            r#"
            SELECT deployment_id, session_id, owner_pod_addr, toolsets, expires_at
              FROM tunnel_registrations
             WHERE expires_at > now()
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(deployment_id, session_id, owner_pod_addr, toolsets, expires_at)| {
                    TunnelRegistrationRow {
                        deployment_id,
                        session_id,
                        owner_pod_addr,
                        toolsets: toolsets.0 .0,
                        expires_at,
                    }
                },
            )
            .collect())
    }

    async fn notify_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        deployment_id: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(NOTIFY_CHANNEL)
            .bind(deployment_id)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }
}

pub const TUNNEL_NOTIFY_CHANNEL: &str = NOTIFY_CHANNEL;
