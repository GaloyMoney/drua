use std::{
    process::Command,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::Context;
use sqlx::{Connection, Executor, PgConnection, Row};

use crate::postgres_mcp::{
    PostgresMcpConfig, PostgresMcpController, PostgresMcpHandler,
    DEFAULT_POSTGRES_MCP_CONFIG_SECRET, DEFAULT_POSTGRES_MCP_IMAGE, DEFAULT_POSTGRES_MCP_LIMIT_CPU,
    DEFAULT_POSTGRES_MCP_LIMIT_MEMORY, DEFAULT_POSTGRES_MCP_MAX_ROWS,
    DEFAULT_POSTGRES_MCP_QUERY_TIMEOUT, DEFAULT_POSTGRES_MCP_REQUEST_CPU,
    DEFAULT_POSTGRES_MCP_REQUEST_MEMORY, DEFAULT_POSTGRES_MCP_UPSTREAM_NAME,
};
use crate::postgres_mcp_postgres::SqlxPostgresSourceDiscoverer;

const LANA_RUNTIME_DB: &str = "lana-bank-lana-bank-main";
const READONLY_USER: &str = "lana_readonly";
const READONLY_PASSWORD: &str = "readonly-password";

#[tokio::test]
#[ignore = "requires Docker or Podman and pulls postgres:17-alpine if missing"]
async fn discovers_recreated_database_from_preexisting_readonly_seed_dsn() -> anyhow::Result<()> {
    let postgres = TestPostgres::start().await?;
    let admin_dsn = postgres.admin_dsn();
    let readonly_seed_dsn = postgres.readonly_seed_dsn();

    let mut admin = connect(&admin_dsn, Duration::from_secs(10)).await?;
    setup_readonly_role(&mut admin).await?;
    create_lana_runtime_database(&mut admin).await?;

    let handler = RecordingPostgresMcpHandler::default();
    let controller = PostgresMcpController::try_new(
        test_config(readonly_seed_dsn),
        handler.clone(),
        SqlxPostgresSourceDiscoverer,
    )?;

    assert!(
        controller.reconcile().await?.is_none(),
        "database without CONNECT grant should not be registered"
    );
    let ungranted = handler.last().expect("recorded ungranted apply");
    assert!(!ungranted.enabled);
    assert!(!ungranted.dbhub_toml.contains("main_runtime"));

    grant_lana_readonly_access(&mut admin, &admin_dsn).await?;
    let first_oid = database_oid(&mut admin).await?;
    assert_readonly_access_policy(&admin_dsn).await?;

    assert!(
        controller.reconcile().await?.is_some(),
        "granted database should be registered"
    );
    let first = handler.last().expect("recorded first apply");
    assert!(first.enabled);
    assert!(first.dbhub_toml.contains("id = \"main_runtime\""));
    assert!(first
        .dbhub_toml
        .contains(&format!("# database_oid = {first_oid}")));

    drop_lana_runtime_database(&mut admin).await?;
    assert!(
        controller.reconcile().await?.is_none(),
        "dropped database should be removed from DBHub config"
    );
    let dropped = handler.last().expect("recorded dropped apply");
    assert!(!dropped.enabled);
    assert!(!dropped.dbhub_toml.contains("main_runtime"));

    create_lana_runtime_database(&mut admin).await?;
    grant_lana_readonly_access(&mut admin, &admin_dsn).await?;
    let second_oid = database_oid(&mut admin).await?;
    assert_readonly_access_policy(&admin_dsn).await?;

    assert_ne!(
        first_oid, second_oid,
        "Postgres should assign a new database OID after drop/recreate"
    );
    assert!(
        controller.reconcile().await?.is_some(),
        "recreated granted database should be registered again"
    );
    let second = handler.last().expect("recorded second apply");
    assert!(second.enabled);
    assert!(second.dbhub_toml.contains("id = \"main_runtime\""));
    assert!(second
        .dbhub_toml
        .contains(&format!("# database_oid = {second_oid}")));
    assert_ne!(
        first.checksum, second.checksum,
        "same-name database recreation must change DBHub checksum"
    );

    Ok(())
}

fn test_config(runtime_seed_dsn: String) -> PostgresMcpConfig {
    PostgresMcpConfig {
        namespace: "test".to_string(),
        resource_name: "lana-postgres-mcp".to_string(),
        config_secret: DEFAULT_POSTGRES_MCP_CONFIG_SECRET.to_string(),
        upstream_name: DEFAULT_POSTGRES_MCP_UPSTREAM_NAME.to_string(),
        image: DEFAULT_POSTGRES_MCP_IMAGE.to_string(),
        image_pull_policy: "IfNotPresent".to_string(),
        service_port: 8000,
        query_timeout: DEFAULT_POSTGRES_MCP_QUERY_TIMEOUT,
        max_rows: DEFAULT_POSTGRES_MCP_MAX_ROWS,
        connect_timeout: Duration::from_secs(5),
        request_cpu: DEFAULT_POSTGRES_MCP_REQUEST_CPU.to_string(),
        request_memory: DEFAULT_POSTGRES_MCP_REQUEST_MEMORY.to_string(),
        limit_cpu: DEFAULT_POSTGRES_MCP_LIMIT_CPU.to_string(),
        limit_memory: DEFAULT_POSTGRES_MCP_LIMIT_MEMORY.to_string(),
        runtime_seed_dsn,
        datawarehouse_seed_dsn: None,
        service_account_name: None,
        cloud_sql_proxy: None,
    }
}

async fn setup_readonly_role(admin: &mut PgConnection) -> anyhow::Result<()> {
    admin
        .execute(
            format!(
                "create role {READONLY_USER} login password '{}'",
                READONLY_PASSWORD.replace('\'', "''")
            )
            .as_str(),
        )
        .await?;
    admin
        .execute(format!("alter role {READONLY_USER} nocreatedb nocreaterole").as_str())
        .await?;
    admin
        .execute(r#"revoke connect on database postgres from public"#)
        .await?;
    admin
        .execute(format!("grant connect on database postgres to {READONLY_USER}").as_str())
        .await?;
    Ok(())
}

async fn create_lana_runtime_database(admin: &mut PgConnection) -> anyhow::Result<()> {
    admin
        .execute(format!(r#"create database "{}""#, LANA_RUNTIME_DB).as_str())
        .await?;
    admin
        .execute(
            format!(
                r#"revoke connect on database "{}" from public"#,
                LANA_RUNTIME_DB
            )
            .as_str(),
        )
        .await?;
    admin
        .execute(
            format!(
                r#"revoke temporary on database "{}" from public"#,
                LANA_RUNTIME_DB
            )
            .as_str(),
        )
        .await?;
    Ok(())
}

async fn grant_lana_readonly_access(
    admin: &mut PgConnection,
    admin_seed_dsn: &str,
) -> anyhow::Result<()> {
    admin
        .execute(
            format!(
                r#"grant connect on database "{}" to {READONLY_USER}"#,
                LANA_RUNTIME_DB
            )
            .as_str(),
        )
        .await?;

    let mut db_admin = connect(
        &dsn_for_database(admin_seed_dsn, LANA_RUNTIME_DB)?,
        Duration::from_secs(10),
    )
    .await?;
    db_admin
        .execute("create table public.tunnel_connector_fixture (id integer primary key)")
        .await?;
    db_admin
        .execute("insert into public.tunnel_connector_fixture (id) values (1)")
        .await?;
    db_admin
        .execute("revoke create on schema public from public")
        .await?;
    db_admin
        .execute(format!("grant usage on schema public to {READONLY_USER}").as_str())
        .await?;
    db_admin
        .execute(format!("grant select on all tables in schema public to {READONLY_USER}").as_str())
        .await?;
    db_admin
        .execute(
            format!("grant select on all sequences in schema public to {READONLY_USER}").as_str(),
        )
        .await?;
    db_admin
        .execute(
            format!(
                "alter default privileges for role postgres in schema public grant select on tables to {READONLY_USER}"
            )
            .as_str(),
        )
        .await?;
    db_admin
        .execute(
            format!(
                "alter default privileges for role postgres in schema public grant select on sequences to {READONLY_USER}"
            )
            .as_str(),
        )
        .await?;

    Ok(())
}

async fn assert_readonly_access_policy(admin_seed_dsn: &str) -> anyhow::Result<()> {
    let mut readonly = connect(
        &readonly_dsn_for_database(admin_seed_dsn, LANA_RUNTIME_DB)?,
        Duration::from_secs(10),
    )
    .await?;
    let row =
        sqlx::query("select count(*)::int8 as row_count from public.tunnel_connector_fixture")
            .fetch_one(&mut readonly)
            .await?;
    let row_count: i64 = row.try_get("row_count")?;

    assert_eq!(row_count, 1);

    let create_table = sqlx::query("create table public.tunnel_connector_forbidden (id integer)")
        .execute(&mut readonly)
        .await;
    assert!(
        create_table.is_err(),
        "readonly role must not create normal tables"
    );

    let create_temp_table =
        sqlx::query("create temporary table tunnel_connector_forbidden_temp (id integer)")
            .execute(&mut readonly)
            .await;
    assert!(
        create_temp_table.is_err(),
        "readonly role must not create temporary tables"
    );

    Ok(())
}

async fn database_oid(admin: &mut PgConnection) -> anyhow::Result<u32> {
    let row = sqlx::query("select oid::int8 as oid from pg_database where datname = $1")
        .bind(LANA_RUNTIME_DB)
        .fetch_one(admin)
        .await?;
    Ok(u32::try_from(row.try_get::<i64, _>("oid")?)?)
}

async fn drop_lana_runtime_database(admin: &mut PgConnection) -> anyhow::Result<()> {
    sqlx::query("select pg_terminate_backend(pid) from pg_stat_activity where datname = $1")
        .bind(LANA_RUNTIME_DB)
        .execute(&mut *admin)
        .await?;
    admin
        .execute(format!(r#"drop database "{}""#, LANA_RUNTIME_DB).as_str())
        .await?;
    Ok(())
}

async fn connect(dsn: &str, timeout: Duration) -> anyhow::Result<PgConnection> {
    tokio::time::timeout(timeout, PgConnection::connect(dsn))
        .await
        .map_err(|_| anyhow::anyhow!("postgres connection timed out after {:?}", timeout))?
        .map_err(Into::into)
}

fn dsn_for_database(seed_dsn: &str, database_name: &str) -> anyhow::Result<String> {
    let mut url = url::Url::parse(seed_dsn)?;
    url.set_path(database_name);
    Ok(url.to_string())
}

fn readonly_dsn_for_database(seed_dsn: &str, database_name: &str) -> anyhow::Result<String> {
    let mut url = url::Url::parse(seed_dsn)?;
    url.set_username(READONLY_USER)
        .map_err(|_| anyhow::anyhow!("failed to set readonly user in dsn"))?;
    url.set_password(Some(READONLY_PASSWORD))
        .map_err(|_| anyhow::anyhow!("failed to set readonly password in dsn"))?;
    url.set_path(database_name);
    Ok(url.to_string())
}

#[derive(Clone, Default)]
struct RecordingPostgresMcpHandler {
    calls: Arc<Mutex<Vec<DbhubApply>>>,
}

impl RecordingPostgresMcpHandler {
    fn last(&self) -> Option<DbhubApply> {
        self.calls.lock().expect("dbhub apply lock").last().cloned()
    }
}

#[derive(Clone)]
struct DbhubApply {
    dbhub_toml: String,
    checksum: String,
    enabled: bool,
}

#[async_trait::async_trait]
impl PostgresMcpHandler for RecordingPostgresMcpHandler {
    async fn apply_dbhub(
        &self,
        _config: &PostgresMcpConfig,
        dbhub_toml: &str,
        checksum: &str,
        enabled: bool,
    ) -> anyhow::Result<()> {
        self.calls
            .lock()
            .expect("dbhub apply lock")
            .push(DbhubApply {
                dbhub_toml: dbhub_toml.to_string(),
                checksum: checksum.to_string(),
                enabled,
            });
        Ok(())
    }
}

struct TestPostgres {
    engine: String,
    name: String,
    port: u16,
}

impl TestPostgres {
    async fn start() -> anyhow::Result<Self> {
        let engine = container_engine()?;
        let port = available_port()?;
        let name = format!(
            "tunnel-connector-postgres-recreate-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let output = Command::new(&engine)
            .args([
                "run",
                "-d",
                "--rm",
                "--name",
                &name,
                "-e",
                "POSTGRES_PASSWORD=postgres",
                "-p",
                &format!("127.0.0.1:{port}:5432"),
                "postgres:17-alpine",
            ])
            .output()?;

        if !output.status.success() {
            anyhow::bail!(
                "failed to start postgres test container with {engine}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let postgres = Self { engine, name, port };
        postgres.wait_until_ready().await?;
        Ok(postgres)
    }

    fn admin_dsn(&self) -> String {
        format!(
            "postgres://postgres:postgres@127.0.0.1:{}/postgres?sslmode=disable",
            self.port
        )
    }

    fn readonly_seed_dsn(&self) -> String {
        format!(
            "postgres://{READONLY_USER}:{READONLY_PASSWORD}@127.0.0.1:{}/postgres?sslmode=disable",
            self.port
        )
    }

    async fn wait_until_ready(&self) -> anyhow::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            match connect(&self.admin_dsn(), Duration::from_secs(2)).await {
                Ok(_) => return Ok(()),
                Err(e) if Instant::now() < deadline => {
                    tracing::debug!(error = %e, "waiting for postgres test container");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(e) => return Err(e).context("postgres test container did not become ready"),
            }
        }
    }
}

impl Drop for TestPostgres {
    fn drop(&mut self) {
        let _ = Command::new(&self.engine)
            .args(["rm", "-f", &self.name])
            .output();
    }
}

fn container_engine() -> anyhow::Result<String> {
    for engine in ["docker", "podman"] {
        if Command::new(engine)
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
        {
            return Ok(engine.to_string());
        }
    }

    anyhow::bail!("docker or podman is required for this ignored e2e test")
}

fn available_port() -> anyhow::Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}
