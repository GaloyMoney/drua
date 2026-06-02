use std::str::FromStr;

use async_trait::async_trait;
use sqlx::{Connection, PgConnection};

use crate::postgres_mcp::{PostgresSource, PostgresSourceValidator};

#[derive(Clone, Default)]
pub(crate) struct SqlxPostgresSourceValidator;

#[async_trait]
impl PostgresSourceValidator for SqlxPostgresSourceValidator {
    async fn validate(
        &self,
        source: &PostgresSource,
        timeout: std::time::Duration,
    ) -> anyhow::Result<()> {
        sqlx::postgres::PgConnectOptions::from_str(&source.dsn)
            .map_err(|e| anyhow::anyhow!("invalid postgres connection string: {e}"))?;

        let mut connection = tokio::time::timeout(timeout, PgConnection::connect(&source.dsn))
            .await
            .map_err(|_| anyhow::anyhow!("postgres connection timed out after {:?}", timeout))??;

        tokio::time::timeout(timeout, sqlx::query("select 1").execute(&mut connection))
            .await
            .map_err(|_| {
                anyhow::anyhow!("postgres validation query timed out after {:?}", timeout)
            })??;

        Ok(())
    }
}
