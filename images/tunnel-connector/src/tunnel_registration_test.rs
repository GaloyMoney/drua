use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use ed25519_dalek::{
    pkcs8::{spki::der::pem::LineEnding, EncodePrivateKey},
    SigningKey,
};
use fake_mcp_upstream::FakeUpstream;
use tokio::sync::{oneshot, watch};

use super::*;
use crate::postgres_mcp::{PostgresSource, RegistryEntry};

fn registration(tool_name: &str) -> RegisteredToolSet {
    RegisteredToolSet {
        name: "postgres".to_string(),
        prefix: "postgres".to_string(),
        category: "deployment".to_string(),
        category_description: "staging deployment".to_string(),
        tools: vec![serde_json::json!({ "name": tool_name })],
    }
}

#[test]
fn registration_fingerprint_changes_when_tools_change() {
    let before = registration_fingerprint(&[registration("execute_sql")]).unwrap();
    let after = registration_fingerprint(&[registration("execute_sql_pg_lana")]).unwrap();

    assert_ne!(before, after);
}

#[tokio::test]
async fn registers_reconciled_postgres_mcp_tools() -> anyhow::Result<()> {
    let (_mcp_dir, postgres_mcp_url) = start_fake_postgres_mcp().await?;
    let postgres_mcp_port = url::Url::parse(&postgres_mcp_url)?
        .port()
        .ok_or_else(|| anyhow::anyhow!("fake postgres mcp url missing port"))?;

    let (tunnel_url, registered_rx) = start_fake_tunnel_server().await?;
    let (_private_key_dir, private_key_file) = write_test_private_key()?;
    let applied_dbhub_toml = Arc::new(Mutex::new(None));
    let handler = FakePostgresMcpHandler {
        registry_yaml: r#"
main:
  runtime_pg_con: "postgres://mcp:secret@postgres.local:5432/lana"
"#
        .to_string(),
        applied_dbhub_toml: applied_dbhub_toml.clone(),
    };

    let postgres_mcp = PostgresMcpController::try_new(
        PostgresMcpConfig {
            namespace: "test".to_string(),
            registry_secret: DEFAULT_POSTGRES_MCP_REGISTRY_SECRET.to_string(),
            registry_key: DEFAULT_POSTGRES_MCP_REGISTRY_KEY.to_string(),
            resource_name: "127.0.0.1".to_string(),
            config_secret: DEFAULT_POSTGRES_MCP_CONFIG_SECRET.to_string(),
            upstream_name: DEFAULT_POSTGRES_MCP_UPSTREAM_NAME.to_string(),
            image: DEFAULT_POSTGRES_MCP_IMAGE.to_string(),
            image_pull_policy: "IfNotPresent".to_string(),
            service_port: postgres_mcp_port,
            query_timeout: DEFAULT_POSTGRES_MCP_QUERY_TIMEOUT,
            max_rows: DEFAULT_POSTGRES_MCP_MAX_ROWS,
            connect_timeout: Duration::from_secs(DEFAULT_POSTGRES_MCP_CONNECT_TIMEOUT_SECS),
            request_cpu: DEFAULT_POSTGRES_MCP_REQUEST_CPU.to_string(),
            request_memory: DEFAULT_POSTGRES_MCP_REQUEST_MEMORY.to_string(),
            limit_cpu: DEFAULT_POSTGRES_MCP_LIMIT_CPU.to_string(),
            limit_memory: DEFAULT_POSTGRES_MCP_LIMIT_MEMORY.to_string(),
        },
        handler,
        AcceptAllPostgresSourceValidator,
    )?;

    let cli = Cli {
        server_url: tunnel_url,
        private_key_file,
        deployment_id: "galoy-staging".to_string(),
        upstreams: "".to_string(),
        tool_refresh_interval_secs: 0,
        postgres_mcp_namespace: Some("test".to_string()),
        postgres_mcp_registry_secret: DEFAULT_POSTGRES_MCP_REGISTRY_SECRET.to_string(),
        postgres_mcp_registry_key: DEFAULT_POSTGRES_MCP_REGISTRY_KEY.to_string(),
        postgres_mcp_resource_name: "127.0.0.1".to_string(),
        postgres_mcp_config_secret: DEFAULT_POSTGRES_MCP_CONFIG_SECRET.to_string(),
        postgres_mcp_upstream_name: DEFAULT_POSTGRES_MCP_UPSTREAM_NAME.to_string(),
        postgres_mcp_image: DEFAULT_POSTGRES_MCP_IMAGE.to_string(),
        postgres_mcp_image_pull_policy: "IfNotPresent".to_string(),
        postgres_mcp_service_port: postgres_mcp_port,
        postgres_mcp_query_timeout: DEFAULT_POSTGRES_MCP_QUERY_TIMEOUT,
        postgres_mcp_max_rows: DEFAULT_POSTGRES_MCP_MAX_ROWS,
        postgres_mcp_connect_timeout_secs: DEFAULT_POSTGRES_MCP_CONNECT_TIMEOUT_SECS,
        postgres_mcp_request_cpu: DEFAULT_POSTGRES_MCP_REQUEST_CPU.to_string(),
        postgres_mcp_request_memory: DEFAULT_POSTGRES_MCP_REQUEST_MEMORY.to_string(),
        postgres_mcp_limit_cpu: DEFAULT_POSTGRES_MCP_LIMIT_CPU.to_string(),
        postgres_mcp_limit_memory: DEFAULT_POSTGRES_MCP_LIMIT_MEMORY.to_string(),
    };
    let static_upstreams = Vec::new();
    let (_postgres_tx, mut postgres_changes) = watch::channel(0_u64);
    let mut backoff = INITIAL_BACKOFF;

    let tunnel_task = tokio::spawn(async move {
        run_tunnel(
            &cli,
            &static_upstreams,
            &postgres_mcp,
            &mut postgres_changes,
            &mut backoff,
        )
        .await
    });

    let registered = tokio::time::timeout(Duration::from_secs(5), registered_rx).await??;
    let TunnelMessage::Register {
        deployment_id,
        toolsets,
    } = registered
    else {
        anyhow::bail!("expected Register frame");
    };

    assert_eq!(deployment_id, "galoy-staging");
    assert_eq!(toolsets.len(), 1);
    assert_eq!(toolsets[0].name, "lana_postgres");
    assert_eq!(toolsets[0].prefix, "lana_postgres");

    let tool_names = toolsets[0]
        .tools
        .iter()
        .filter_map(|tool| tool.get("name").and_then(serde_json::Value::as_str))
        .collect::<Vec<_>>();
    assert!(tool_names.contains(&"execute_sql_main_runtime"));
    assert!(tool_names.contains(&"search_objects_main_runtime"));

    let dbhub_toml = applied_dbhub_toml
        .lock()
        .expect("applied dbhub config lock")
        .clone()
        .expect("dbhub config was applied");
    assert!(dbhub_toml.contains("id = \"main_runtime\""));
    assert!(dbhub_toml.contains("source = \"main_runtime\""));

    tokio::time::timeout(Duration::from_secs(5), tunnel_task).await???;

    Ok(())
}

#[derive(Clone)]
struct FakePostgresMcpHandler {
    registry_yaml: String,
    applied_dbhub_toml: Arc<Mutex<Option<String>>>,
}

#[async_trait::async_trait]
impl PostgresMcpHandler for FakePostgresMcpHandler {
    fn spawn_registry_watcher(&self, _config: &PostgresMcpConfig) -> watch::Receiver<u64> {
        let (_tx, rx) = watch::channel(0_u64);
        rx
    }

    async fn read_registry(
        &self,
        _config: &PostgresMcpConfig,
    ) -> anyhow::Result<BTreeMap<String, RegistryEntry>> {
        crate::postgres_mcp::parse_registry_yaml(&self.registry_yaml)
    }

    async fn apply_dbhub(
        &self,
        _config: &PostgresMcpConfig,
        dbhub_toml: &str,
        _checksum: &str,
        enabled: bool,
    ) -> anyhow::Result<()> {
        assert!(enabled, "expected valid registry source to enable DBHub");
        *self.applied_dbhub_toml.lock().expect("dbhub config lock") = Some(dbhub_toml.to_string());
        Ok(())
    }
}

#[derive(Clone)]
struct AcceptAllPostgresSourceValidator;

#[async_trait::async_trait]
impl PostgresSourceValidator for AcceptAllPostgresSourceValidator {
    async fn validate(&self, _source: &PostgresSource, _timeout: Duration) -> anyhow::Result<()> {
        Ok(())
    }
}

async fn start_fake_postgres_mcp() -> anyhow::Result<(TestDir, String)> {
    let dir = TestDir::new();
    dir.write_fixture("execute_sql_main_runtime")?;
    dir.write_fixture("search_objects_main_runtime")?;

    let upstream = FakeUpstream::load(dir.path())?;
    let app = Router::new().nest_service("/mcp", upstream.into_service());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let local_addr = listener.local_addr()?;

    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    Ok((dir, format!("http://{local_addr}/mcp")))
}

async fn start_fake_tunnel_server() -> anyhow::Result<(String, oneshot::Receiver<TunnelMessage>)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let local_addr = listener.local_addr()?;
    let (tx, rx) = oneshot::channel();

    tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
            return;
        };
        let mut tx = Some(tx);

        while let Some(Ok(msg)) = ws.next().await {
            if let tungstenite::Message::Text(text) = msg {
                if let Ok(parsed) = serde_json::from_str::<TunnelMessage>(&text) {
                    if let Some(tx) = tx.take() {
                        let _ = tx.send(parsed);
                    }
                }
                let _ = ws.close(None).await;
                break;
            }
        }
    });

    Ok((format!("ws://{local_addr}/tunnel/ws"), rx))
}

fn write_test_private_key() -> anyhow::Result<(TestDir, std::path::PathBuf)> {
    let dir = TestDir::new();
    let path = dir.path().join("private_key.pem");
    let signing_key = SigningKey::from_bytes(&[7_u8; 32]);
    let pem = signing_key.to_pkcs8_pem(LineEnding::LF)?;
    std::fs::write(&path, pem.as_str())?;
    Ok((dir, path))
}

struct TestDir {
    path: std::path::PathBuf,
}

impl TestDir {
    fn new() -> Self {
        let nonce = format!(
            "{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let path = std::env::temp_dir().join(format!("tunnel-connector-test-{nonce}"));
        std::fs::create_dir_all(&path).expect("create temp test dir");
        Self { path }
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn write_fixture(&self, name: &str) -> anyhow::Result<()> {
        let body = serde_json::json!({
            "name": name,
            "description": format!("fake DBHub tool {name}"),
            "upstream": {
                "content": [{
                    "type": "text",
                    "text": "ok"
                }]
            }
        });
        std::fs::write(
            self.path.join(format!("{name}.json")),
            serde_json::to_vec(&body)?,
        )?;
        Ok(())
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
