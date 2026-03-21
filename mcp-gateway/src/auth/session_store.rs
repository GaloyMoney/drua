use async_trait::async_trait;
use tower_sessions::cookie::SameSite;
use tower_sessions::session::{Id, Record};
use tower_sessions::{session_store, Expiry, SessionManagerLayer};

/// Postgres-backed session store for tower-sessions.
///
/// Uses raw sqlx queries with integer timestamps to avoid
/// chrono/time feature conflicts with es-entity.
#[derive(Clone, Debug)]
pub struct PgSessionStore {
    pool: sqlx::PgPool,
}

impl PgSessionStore {
    pub fn new(pool: &sqlx::PgPool) -> Self {
        Self { pool: pool.clone() }
    }

    /// Returns a [`SessionManagerLayer`] configured with secure defaults:
    /// `SameSite=Lax`, `HttpOnly=true`, `Secure=true`, 24-hour inactivity expiry.
    pub fn into_layer(self) -> SessionManagerLayer<Self> {
        SessionManagerLayer::new(self)
            .with_same_site(SameSite::Lax)
            .with_http_only(true)
            .with_secure(true)
            .with_expiry(Expiry::OnInactivity(time::Duration::hours(24)))
    }
}

#[async_trait]
impl session_store::SessionStore for PgSessionStore {
    async fn save(&self, record: &Record) -> session_store::Result<()> {
        let data = serde_json::to_value(&record.data)
            .map_err(|e| session_store::Error::Backend(e.to_string()))?;
        let expiry = record.expiry_date.unix_timestamp();

        sqlx::query(
            r#"INSERT INTO sessions (id, data, expiry_date)
               VALUES ($1, $2, $3)
               ON CONFLICT (id) DO UPDATE SET data = $2, expiry_date = $3"#,
        )
        .bind(record.id.to_string())
        .bind(&data)
        .bind(expiry)
        .execute(&self.pool)
        .await
        .map_err(|e| session_store::Error::Backend(e.to_string()))?;

        Ok(())
    }

    async fn load(&self, session_id: &Id) -> session_store::Result<Option<Record>> {
        let row: Option<(String, serde_json::Value, i64)> = sqlx::query_as(
            r#"SELECT id, data, expiry_date FROM sessions
               WHERE id = $1 AND expiry_date > $2"#,
        )
        .bind(session_id.to_string())
        .bind(time::OffsetDateTime::now_utc().unix_timestamp())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| session_store::Error::Backend(e.to_string()))?;

        match row {
            Some((id, data, expiry)) => {
                let session_id: Id = id
                    .parse()
                    .map_err(|_| session_store::Error::Backend("invalid session id".to_string()))?;
                let data = serde_json::from_value(data)
                    .map_err(|e| session_store::Error::Backend(e.to_string()))?;
                let expiry_date = time::OffsetDateTime::from_unix_timestamp(expiry)
                    .map_err(|e| session_store::Error::Backend(e.to_string()))?;

                Ok(Some(Record {
                    id: session_id,
                    data,
                    expiry_date,
                }))
            }
            None => Ok(None),
        }
    }

    async fn delete(&self, session_id: &Id) -> session_store::Result<()> {
        sqlx::query("DELETE FROM sessions WHERE id = $1")
            .bind(session_id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| session_store::Error::Backend(e.to_string()))?;

        Ok(())
    }
}
