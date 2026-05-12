//! `tunnel_registrations` table accessor — the source of truth for
//! "which pod terminates which tunnel WebSocket". Owned-side mutations
//! emit `pg_notify('tunnel_registry_changed', deployment_id)` in the
//! same transaction; the listener on every pod reads and reconciles
//! into `ProxyTunnelToolSet` entries.
//!
//! Heartbeat-driven displacement: the new owner's UPSERT changes
//! `session_id`, so the prior owner's `heartbeat()` hits zero
//! rows-affected and the WS loop closes itself. No pg_notify required
//! for that signal — `Ok(false)` from heartbeat is authoritative.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};

use super::RegisteredToolSet;

const NOTIFY_CHANNEL: &str = "tunnel_registry_changed";

#[derive(Debug, Clone)]
pub struct TunnelRegistrationRow {
    pub deployment_id: String,
    pub session_id: uuid::Uuid,
    pub owner_pod_addr: String,
    pub toolsets: Vec<RegisteredToolSet>,
    pub expires_at: DateTime<Utc>,
}

/// JSONB-serialized payload of `tunnel_registrations.toolsets`. We
/// store the connector's full `RegisteredToolSet[]` so peer pods can
/// answer `search_tools`/`describe_tool` without re-querying anything.
#[derive(Debug, Serialize, Deserialize)]
struct ToolsetsPayload(Vec<RegisteredToolSet>);

/// SQLx `query_as` row tuple — typed for clippy::type_complexity.
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

    /// Insert or take over the row for `deployment_id`. Sets
    /// `session_id`, `owner_pod_addr`, the toolset catalog and an
    /// `expires_at` based on `ttl`. The displaced previous owner finds
    /// out via its next heartbeat (rows-affected = 0).
    pub async fn upsert(
        &self,
        deployment_id: &str,
        session_id: uuid::Uuid,
        owner_pod_addr: &str,
        toolsets: &[RegisteredToolSet],
        ttl: Duration,
    ) -> Result<(), sqlx::Error> {
        let payload = serde_json::to_value(ToolsetsPayload(toolsets.to_vec()))
            .expect("RegisteredToolSet serialization is infallible");
        let ttl_secs = ttl.as_secs() as i64;
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r#"
            INSERT INTO tunnel_registrations
                (deployment_id, session_id, owner_pod_addr, toolsets, expires_at)
            VALUES
                ($1, $2, $3, $4, now() + make_interval(secs => $5::int))
            ON CONFLICT (deployment_id) DO UPDATE
                SET session_id     = EXCLUDED.session_id,
                    owner_pod_addr = EXCLUDED.owner_pod_addr,
                    toolsets       = EXCLUDED.toolsets,
                    registered_at  = now(),
                    expires_at     = EXCLUDED.expires_at
            "#,
        )
        .bind(deployment_id)
        .bind(session_id)
        .bind(owner_pod_addr)
        .bind(&payload)
        .bind(ttl_secs)
        .execute(&mut *tx)
        .await?;

        Self::notify_in_tx(&mut tx, deployment_id).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Extend `expires_at` if and only if `(deployment_id, session_id)`
    /// still matches. Returns `Ok(true)` on extend, `Ok(false)` if the
    /// row was taken over (or never existed) — caller treats that as
    /// displacement and closes its WebSocket.
    pub async fn heartbeat(
        &self,
        deployment_id: &str,
        session_id: uuid::Uuid,
        ttl: Duration,
    ) -> Result<bool, sqlx::Error> {
        let ttl_secs = ttl.as_secs() as i64;
        let result = sqlx::query(
            r#"
            UPDATE tunnel_registrations
               SET expires_at = now() + make_interval(secs => $3::int)
             WHERE deployment_id = $1 AND session_id = $2
            "#,
        )
        .bind(deployment_id)
        .bind(session_id)
        .bind(ttl_secs)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Delete only if `(deployment_id, session_id)` still matches.
    /// Returns rows-affected so callers can log on stale-session no-ops.
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

    /// Drop every row this pod owns. Called from `App::shutdown` so a
    /// rolling restart hands ownership over within an HTTP round-trip
    /// rather than waiting for the reaper.
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

    /// Reaper sweep — idempotent; safe to run on every pod.
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

    /// Fetch one row, treating expired rows as absent so peer pods never
    /// route through a tunnel the reaper is about to drop.
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

    /// All non-expired rows. Used by the listener for its initial sweep
    /// after (re)connecting to PG.
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

pub(crate) const TUNNEL_NOTIFY_CHANNEL: &str = NOTIFY_CHANNEL;
