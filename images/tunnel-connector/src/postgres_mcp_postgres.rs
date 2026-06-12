use std::str::FromStr;

use async_trait::async_trait;
use sqlx::{Connection, PgConnection, Row};

use crate::postgres_mcp::{
    PostgresMcpConfig, PostgresSource, PostgresSourceDiscoverer, PostgresSourceRole,
};

#[derive(Clone, Default)]
pub(crate) struct SqlxPostgresSourceDiscoverer;

#[async_trait]
impl PostgresSourceDiscoverer for SqlxPostgresSourceDiscoverer {
    async fn discover_sources(
        &self,
        config: &PostgresMcpConfig,
    ) -> anyhow::Result<Vec<PostgresSource>> {
        let mut sources = Vec::new();

        sources.extend(
            discover_instance_sources(
                &config.runtime_seed_dsn,
                PostgresSourceRole::Runtime,
                config.connect_timeout,
            )
            .await,
        );

        if let Some(seed_dsn) = config.datawarehouse_seed_dsn.as_deref() {
            sources.extend(
                discover_instance_sources(
                    seed_dsn,
                    PostgresSourceRole::Datawarehouse,
                    config.connect_timeout,
                )
                .await,
            );
        }

        sources.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(sources)
    }
}

async fn discover_instance_sources(
    seed_dsn: &str,
    role: PostgresSourceRole,
    timeout: std::time::Duration,
) -> Vec<PostgresSource> {
    match try_discover_instance_sources(seed_dsn, role.clone(), timeout).await {
        Ok(sources) => sources,
        Err(e) => {
            tracing::warn!(
                role = %role.suffix(),
                error = %e,
                "postgres mcp discovery failed for seed dsn; omitting this postgres instance"
            );
            Vec::new()
        }
    }
}

async fn try_discover_instance_sources(
    seed_dsn: &str,
    role: PostgresSourceRole,
    timeout: std::time::Duration,
) -> anyhow::Result<Vec<PostgresSource>> {
    validate_dsn(seed_dsn)?;

    let mut seed_connection = connect(seed_dsn, timeout).await?;
    let rows = tokio::time::timeout(
        timeout,
        sqlx::query(
            r#"
            select oid::int8 as database_oid, datname
            from pg_database
            where datallowconn
              and not datistemplate
              and has_database_privilege(datname, 'CONNECT')
            order by datname
            "#,
        )
        .fetch_all(&mut seed_connection),
    )
    .await
    .map_err(|_| anyhow::anyhow!("postgres database discovery timed out after {:?}", timeout))??;

    let mut sources = Vec::new();
    for row in rows {
        let database_oid = u32::try_from(row.try_get::<i64, _>("database_oid")?)
            .map_err(|_| anyhow::anyhow!("postgres database oid is out of range"))?;
        let database_name: String = row.try_get("datname")?;
        let Ok(dsn) = dsn_for_database(seed_dsn, &database_name) else {
            tracing::warn!(
                database = %database_name,
                "postgres mcp discovery could not rewrite seed dsn for database"
            );
            continue;
        };

        let Some(source) =
            PostgresSource::from_database_name(&database_name, role.clone(), dsn, database_oid)
        else {
            continue;
        };

        match validate_source(&source, timeout).await {
            Ok(()) => sources.push(source),
            Err(e) => {
                tracing::warn!(
                    instance = %source.instance,
                    source = %source.id,
                    error = %e,
                    "omitting unusable postgres mcp source"
                );
            }
        }
    }

    Ok(sources)
}

async fn validate_source(
    source: &PostgresSource,
    timeout: std::time::Duration,
) -> anyhow::Result<()> {
    let mut connection = connect(&source.dsn, timeout).await?;
    tokio::time::timeout(timeout, sqlx::query("select 1").execute(&mut connection))
        .await
        .map_err(|_| {
            anyhow::anyhow!("postgres validation query timed out after {:?}", timeout)
        })??;
    Ok(())
}

async fn connect(dsn: &str, timeout: std::time::Duration) -> anyhow::Result<PgConnection> {
    tokio::time::timeout(timeout, PgConnection::connect(dsn))
        .await
        .map_err(|_| anyhow::anyhow!("postgres connection timed out after {:?}", timeout))?
        .map_err(Into::into)
}

fn validate_dsn(dsn: &str) -> anyhow::Result<()> {
    sqlx::postgres::PgConnectOptions::from_str(dsn)
        .map_err(|e| anyhow::anyhow!("invalid postgres connection string: {e}"))?;
    Ok(())
}

fn dsn_for_database(seed_dsn: &str, database_name: &str) -> anyhow::Result<String> {
    let mut url = url::Url::parse(seed_dsn)?;
    url.set_path(database_name);
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_seed_database_without_losing_query() {
        let rewritten = dsn_for_database(
            "postgres://lana_readonly:secret@127.0.0.1:5432/postgres?sslmode=require",
            "lana-bank-lana-bank-main",
        )
        .unwrap();

        assert_eq!(
            rewritten,
            "postgres://lana_readonly:secret@127.0.0.1:5432/lana-bank-lana-bank-main?sslmode=require"
        );
    }

    #[test]
    fn rewrites_iam_seed_database_without_losing_role_option() {
        let rewritten = dsn_for_database(
            "postgres://mcp@galoystaging.iam@127.0.0.1:5432/postgres?sslmode=disable&options=-c%20role%3Dlana_readonly",
            "lana-bank-lana-bank-main",
        )
        .unwrap();

        assert_eq!(
            rewritten,
            "postgres://mcp%40galoystaging.iam@127.0.0.1:5432/lana-bank-lana-bank-main?sslmode=disable&options=-c%20role%3Dlana_readonly"
        );
    }
}
