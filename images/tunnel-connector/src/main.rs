//! Tunnel connector — runs in a target cluster, discovers MCP tools
//! locally, dials out to drua over WSS with an Ed25519-signed handshake,
//! relays tool calls. Reconnects with exponential backoff + jitter.

use std::sync::Arc;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use clap::Parser;
use ed25519_dalek::{pkcs8::DecodePrivateKey, Signer, SigningKey};
use futures::{SinkExt, StreamExt};
use mcp_upstream::{
    call_tool, discover_upstream, parse_upstreams, registration_fingerprint, tool_catalog_changed,
    McpClients, RegisteredToolSet, UpstreamConfig,
};
use postgres_mcp::{
    PostgresMcpConfig, PostgresMcpController, PostgresMcpHandler, PostgresSourceValidator,
    DEFAULT_POSTGRES_MCP_CONFIG_SECRET, DEFAULT_POSTGRES_MCP_CONNECT_TIMEOUT_SECS,
    DEFAULT_POSTGRES_MCP_IMAGE, DEFAULT_POSTGRES_MCP_LIMIT_CPU, DEFAULT_POSTGRES_MCP_LIMIT_MEMORY,
    DEFAULT_POSTGRES_MCP_MAX_ROWS, DEFAULT_POSTGRES_MCP_QUERY_TIMEOUT,
    DEFAULT_POSTGRES_MCP_REGISTRY_KEY, DEFAULT_POSTGRES_MCP_REGISTRY_SECRET,
    DEFAULT_POSTGRES_MCP_REQUEST_CPU, DEFAULT_POSTGRES_MCP_REQUEST_MEMORY,
    DEFAULT_POSTGRES_MCP_RESOURCE_NAME, DEFAULT_POSTGRES_MCP_SERVICE_PORT,
    DEFAULT_POSTGRES_MCP_UPSTREAM_NAME,
};
use postgres_mcp_kubernetes::{resolve_namespace, KubernetesPostgresMcpHandler};
use postgres_mcp_postgres::SqlxPostgresSourceValidator;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite;

mod mcp_upstream;
mod postgres_mcp;
mod postgres_mcp_kubernetes;
mod postgres_mcp_postgres;
#[cfg(test)]
mod tunnel_registration_test;

type ManagedPostgresMcpController =
    PostgresMcpController<KubernetesPostgresMcpHandler, SqlxPostgresSourceValidator>;

// ---------------------------------------------------------------------------
// Wire protocol (mirrored from galoy-agents-core::tunnel)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TunnelMessage {
    Register {
        deployment_id: String,
        toolsets: Vec<RegisteredToolSet>,
    },
    CallTool {
        id: String,
        upstream: String,
        tool_name: String,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    },
    CallToolResult {
        id: String,
        result: serde_json::Value,
    },
    CallToolError {
        id: String,
        error: String,
    },
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "tunnel-connector",
    about = "Outbound tunnel from a deployment cluster to drua"
)]
struct Cli {
    /// drua tunnel WebSocket URL
    #[arg(long, env = "TUNNEL_SERVER_URL")]
    server_url: String,

    /// Path to the PEM-encoded Ed25519 private key that identifies this
    /// deployment. The matching public key must be registered on drua
    /// under `server.tunnel.deployments.<deployment_id>`. Mounted from
    /// a Kubernetes Secret in production.
    #[arg(long, env = "TUNNEL_PRIVATE_KEY_FILE")]
    private_key_file: std::path::PathBuf,

    /// Deployment identifier (e.g. "galoy-staging"). Must match the
    /// key under `server.tunnel.deployments` in drua's config.
    #[arg(long, env = "TUNNEL_DEPLOYMENT_ID")]
    deployment_id: String,

    /// Comma-separated upstream MCP servers: name=url[,name=url,...].
    /// May be empty when the Postgres MCP registry has valid sources.
    #[arg(long, env = "TUNNEL_UPSTREAMS", default_value = "")]
    upstreams: String,

    /// Poll interval for upstream MCP tool catalog changes. A value of 0 disables refresh.
    #[arg(
        long,
        env = "TUNNEL_TOOL_REFRESH_INTERVAL_SECS",
        default_value_t = DEFAULT_TOOL_REFRESH_INTERVAL_SECS
    )]
    tool_refresh_interval_secs: u64,

    /// Namespace containing the Lana-written registry Secret and generated DBHub resources.
    /// Defaults to the pod namespace when running in Kubernetes.
    #[arg(long, env = "TUNNEL_POSTGRES_MCP_NAMESPACE")]
    postgres_mcp_namespace: Option<String>,

    /// Lana-written Secret containing the Postgres MCP registry YAML.
    #[arg(
        long,
        env = "TUNNEL_POSTGRES_MCP_REGISTRY_SECRET",
        default_value = DEFAULT_POSTGRES_MCP_REGISTRY_SECRET
    )]
    postgres_mcp_registry_secret: String,

    /// Secret data key that stores the registry YAML.
    #[arg(
        long,
        env = "TUNNEL_POSTGRES_MCP_REGISTRY_KEY",
        default_value = DEFAULT_POSTGRES_MCP_REGISTRY_KEY
    )]
    postgres_mcp_registry_key: String,

    /// Fixed name for the generated aggregate DBHub Deployment and Service.
    #[arg(
        long,
        env = "TUNNEL_POSTGRES_MCP_RESOURCE_NAME",
        default_value = DEFAULT_POSTGRES_MCP_RESOURCE_NAME
    )]
    postgres_mcp_resource_name: String,

    /// Fixed name for the generated DBHub config Secret.
    #[arg(
        long,
        env = "TUNNEL_POSTGRES_MCP_CONFIG_SECRET",
        default_value = DEFAULT_POSTGRES_MCP_CONFIG_SECRET
    )]
    postgres_mcp_config_secret: String,

    /// Drua upstream name used for the aggregate DBHub MCP service.
    #[arg(
        long,
        env = "TUNNEL_POSTGRES_MCP_UPSTREAM_NAME",
        default_value = DEFAULT_POSTGRES_MCP_UPSTREAM_NAME
    )]
    postgres_mcp_upstream_name: String,

    /// DBHub image used for the aggregate Postgres MCP server.
    #[arg(
        long,
        env = "TUNNEL_POSTGRES_MCP_IMAGE",
        default_value = DEFAULT_POSTGRES_MCP_IMAGE
    )]
    postgres_mcp_image: String,

    /// Image pull policy for the DBHub container.
    #[arg(
        long,
        env = "TUNNEL_POSTGRES_MCP_IMAGE_PULL_POLICY",
        default_value = "IfNotPresent"
    )]
    postgres_mcp_image_pull_policy: String,

    /// DBHub HTTP port.
    #[arg(
        long,
        env = "TUNNEL_POSTGRES_MCP_SERVICE_PORT",
        default_value_t = DEFAULT_POSTGRES_MCP_SERVICE_PORT
    )]
    postgres_mcp_service_port: u16,

    /// DBHub query timeout, in seconds, rendered into each source.
    #[arg(
        long,
        env = "TUNNEL_POSTGRES_MCP_QUERY_TIMEOUT",
        default_value_t = DEFAULT_POSTGRES_MCP_QUERY_TIMEOUT
    )]
    postgres_mcp_query_timeout: u32,

    /// DBHub maximum rows returned by execute_sql.
    #[arg(
        long,
        env = "TUNNEL_POSTGRES_MCP_MAX_ROWS",
        default_value_t = DEFAULT_POSTGRES_MCP_MAX_ROWS
    )]
    postgres_mcp_max_rows: u32,

    /// Timeout for per-source Postgres credential checks.
    #[arg(
        long,
        env = "TUNNEL_POSTGRES_MCP_CONNECT_TIMEOUT_SECS",
        default_value_t = DEFAULT_POSTGRES_MCP_CONNECT_TIMEOUT_SECS
    )]
    postgres_mcp_connect_timeout_secs: u64,

    /// CPU request for generated DBHub pods.
    #[arg(
        long,
        env = "TUNNEL_POSTGRES_MCP_REQUEST_CPU",
        default_value = DEFAULT_POSTGRES_MCP_REQUEST_CPU
    )]
    postgres_mcp_request_cpu: String,

    /// Memory request for generated DBHub pods.
    #[arg(
        long,
        env = "TUNNEL_POSTGRES_MCP_REQUEST_MEMORY",
        default_value = DEFAULT_POSTGRES_MCP_REQUEST_MEMORY
    )]
    postgres_mcp_request_memory: String,

    /// CPU limit for generated DBHub pods.
    #[arg(
        long,
        env = "TUNNEL_POSTGRES_MCP_LIMIT_CPU",
        default_value = DEFAULT_POSTGRES_MCP_LIMIT_CPU
    )]
    postgres_mcp_limit_cpu: String,

    /// Memory limit for generated DBHub pods.
    #[arg(
        long,
        env = "TUNNEL_POSTGRES_MCP_LIMIT_MEMORY",
        default_value = DEFAULT_POSTGRES_MCP_LIMIT_MEMORY
    )]
    postgres_mcp_limit_memory: String,
}

fn load_signing_key(path: &std::path::Path) -> anyhow::Result<SigningKey> {
    let pem = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading private key from {}: {e}", path.display()))?;
    SigningKey::from_pkcs8_pem(&pem)
        .map_err(|e| anyhow::anyhow!("private key is not PKCS#8 Ed25519 PEM: {e}"))
}

/// `Authorization: Tunnel <deployment_id>:<ts_ms>:<sig>`. Fresh ts per
/// call so a stolen header can't outlive drua's replay window.
fn sign_handshake(deployment_id: &str, signing_key: &SigningKey) -> String {
    let ts_ms = chrono::Utc::now().timestamp_millis();
    let payload = format!("{deployment_id}|{ts_ms}");
    let sig = signing_key.sign(payload.as_bytes());
    let sig_b64 = URL_SAFE_NO_PAD.encode(sig.to_bytes());
    format!("Tunnel {deployment_id}:{ts_ms}:{sig_b64}")
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

// Exp backoff with jitter so a fleet of connectors doesn't resonate on
// simultaneous drua rollout.
const INITIAL_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);
const MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(60);
const JITTER_MS_MAX: u64 = 1_000;
const DEFAULT_TOOL_REFRESH_INTERVAL_SECS: u64 = 300;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
    let static_upstreams = parse_upstreams(&cli.upstreams);
    let postgres_mcp = build_postgres_mcp(&cli).await?;
    let mut postgres_changes = postgres_mcp.spawn_registry_watcher();

    let mut backoff = INITIAL_BACKOFF;
    loop {
        // `run_tunnel` resets `backoff` to `INITIAL_BACKOFF` once it has
        // successfully sent the Register frame. That way a long-lived
        // session that eventually errors out doesn't start its *next*
        // reconnect from a stale high backoff.
        match run_tunnel(
            &cli,
            &static_upstreams,
            postgres_mcp.as_ref(),
            &mut postgres_changes,
            &mut backoff,
        )
        .await
        {
            Ok(()) => tracing::info!("tunnel closed cleanly"),
            Err(e) => tracing::error!(error = %e, "tunnel session failed"),
        }
        let jitter = std::time::Duration::from_millis(rand::random::<u64>() % JITTER_MS_MAX);
        let delay = backoff + jitter;
        tracing::info!(delay_ms = %delay.as_millis(), "reconnecting");
        tokio::time::sleep(delay).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

async fn build_postgres_mcp(cli: &Cli) -> anyhow::Result<Arc<ManagedPostgresMcpController>> {
    let config = PostgresMcpConfig {
        namespace: resolve_namespace(cli.postgres_mcp_namespace.clone())?,
        registry_secret: cli.postgres_mcp_registry_secret.clone(),
        registry_key: cli.postgres_mcp_registry_key.clone(),
        resource_name: cli.postgres_mcp_resource_name.clone(),
        config_secret: cli.postgres_mcp_config_secret.clone(),
        upstream_name: cli.postgres_mcp_upstream_name.clone(),
        image: cli.postgres_mcp_image.clone(),
        image_pull_policy: cli.postgres_mcp_image_pull_policy.clone(),
        service_port: cli.postgres_mcp_service_port,
        query_timeout: cli.postgres_mcp_query_timeout,
        max_rows: cli.postgres_mcp_max_rows,
        connect_timeout: std::time::Duration::from_secs(cli.postgres_mcp_connect_timeout_secs),
        request_cpu: cli.postgres_mcp_request_cpu.clone(),
        request_memory: cli.postgres_mcp_request_memory.clone(),
        limit_cpu: cli.postgres_mcp_limit_cpu.clone(),
        limit_memory: cli.postgres_mcp_limit_memory.clone(),
    };

    let handler = KubernetesPostgresMcpHandler::try_default().await?;
    let validator = SqlxPostgresSourceValidator;

    Ok(Arc::new(PostgresMcpController::try_new(
        config, handler, validator,
    )?))
}

async fn run_tunnel<H, V>(
    cli: &Cli,
    static_upstreams: &[UpstreamConfig],
    postgres_mcp: &PostgresMcpController<H, V>,
    postgres_changes: &mut watch::Receiver<u64>,
    backoff: &mut std::time::Duration,
) -> anyhow::Result<()>
where
    H: PostgresMcpHandler,
    V: PostgresSourceValidator,
{
    let upstreams = build_upstreams(static_upstreams, postgres_mcp).await?;
    if upstreams.is_empty() {
        anyhow::bail!("all configured MCP upstreams are unavailable or disabled");
    }

    // ── 1. Discover tools from local MCP servers ──────────────────────────
    let mut mcp_clients = McpClients::new();
    let mut registrations: Vec<RegisteredToolSet> = Vec::new();
    let postgres_upstream_name = postgres_mcp.upstream_name();

    for upstream in &upstreams {
        match discover_upstream(upstream, &cli.deployment_id).await {
            Ok((name, client, registration)) => {
                registrations.push(registration);
                mcp_clients.insert(name, client);
            }
            Err(e) => {
                if postgres_upstream_name == upstream.name.as_str() {
                    anyhow::bail!(
                        "postgres mcp upstream {} is unavailable after reconciliation: {e}",
                        upstream.name
                    );
                }

                tracing::warn!(
                    name = %upstream.name,
                    url = %upstream.url,
                    error = %e,
                    "skipping unavailable MCP upstream"
                );
            }
        }
    }

    if registrations.is_empty() {
        anyhow::bail!("all configured MCP upstreams are unavailable");
    }

    // ── 2. Connect WebSocket to drua ──────────────────────────────────────
    // Sign a fresh handshake header per connect attempt — timestamp is
    // in the signed payload, so drua's replay-window check naturally
    // rejects a stolen header once it's ~60s old.
    let signing_key = load_signing_key(&cli.private_key_file)?;
    let authorization = sign_handshake(&cli.deployment_id, &signing_key);

    let parsed_url = url::Url::parse(&cli.server_url)?;
    let host = parsed_url.host_str().unwrap_or("localhost").to_string();

    let request = tungstenite::http::Request::builder()
        .uri(&cli.server_url)
        .header("Authorization", authorization)
        .header("Host", &host)
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header(
            "Sec-WebSocket-Key",
            tungstenite::handshake::client::generate_key(),
        )
        .body(())?;

    let (ws_stream, _response) = tokio_tungstenite::connect_async(request).await?;
    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    tracing::info!(server = %cli.server_url, "connected to drua");

    // ── 3. Send registration ──────────────────────────────────────────────
    let registration_fingerprint = registration_fingerprint(&registrations)?;
    let register_msg = TunnelMessage::Register {
        deployment_id: cli.deployment_id.clone(),
        toolsets: registrations,
    };
    let json = serde_json::to_string(&register_msg)?;
    ws_tx.send(tungstenite::Message::Text(json.into())).await?;
    tracing::info!("registration sent");

    // Past the connect-and-register gauntlet — this session is working.
    // Reset reconnect backoff so a later in-session failure starts the
    // next attempt fresh, instead of inheriting stale doubling.
    *backoff = INITIAL_BACKOFF;

    // ── 4. Relay loop ─────────────────────────────────────────────────────
    let refresh_enabled = cli.tool_refresh_interval_secs > 0;
    let refresh_interval = std::time::Duration::from_secs(cli.tool_refresh_interval_secs.max(1));
    let mut refresh_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + refresh_interval,
        refresh_interval,
    );
    refresh_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;
            _ = refresh_tick.tick(), if refresh_enabled => {
                match build_upstreams(static_upstreams, postgres_mcp).await {
                    Ok(refreshed_upstreams) if refreshed_upstreams != upstreams => {
                        tracing::info!("configured upstream set changed; reconnecting to refresh drua registration");
                        return Ok(());
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "upstream reconciliation refresh failed; keeping current registration");
                    }
                }

                match tool_catalog_changed(&upstreams, &cli.deployment_id, &registration_fingerprint).await {
                    Ok(true) => {
                        tracing::info!("upstream tool catalog changed; reconnecting to refresh drua registration");
                        return Ok(());
                    }
                    Ok(false) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "upstream tool catalog refresh failed; keeping current registration");
                    }
                }
            }
            _ = async {
                let _ = postgres_changes.changed().await;
            } => {
                tracing::info!("postgres mcp registry changed; reconnecting to refresh drua registration");
                return Ok(());
            }
            maybe_msg = ws_rx.next() => {
                let Some(msg) = maybe_msg else {
                    break;
                };
                let msg = msg?;
                match msg {
                    tungstenite::Message::Text(text) => {
                        let tunnel_msg: TunnelMessage = match serde_json::from_str(&text) {
                            Ok(m) => m,
                            Err(e) => {
                                tracing::warn!(error = %e, "ignoring unparseable message");
                                continue;
                            }
                        };

                        match tunnel_msg {
                            TunnelMessage::CallTool {
                                id,
                                upstream,
                                tool_name,
                                arguments,
                            } => {
                                let response =
                                    handle_call(&mcp_clients, id, &upstream, &tool_name, arguments).await;
                                let json = serde_json::to_string(&response)?;
                                ws_tx.send(tungstenite::Message::Text(json.into())).await?;
                            }
                            _ => {
                                tracing::warn!("unexpected message type from server");
                            }
                        }
                    }
                    tungstenite::Message::Ping(data) => {
                        ws_tx.send(tungstenite::Message::Pong(data)).await?;
                    }
                    tungstenite::Message::Close(_) => {
                        tracing::info!("server closed connection");
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

async fn build_upstreams<H, V>(
    static_upstreams: &[UpstreamConfig],
    postgres_mcp: &PostgresMcpController<H, V>,
) -> anyhow::Result<Vec<UpstreamConfig>>
where
    H: PostgresMcpHandler,
    V: PostgresSourceValidator,
{
    let mut upstreams = static_upstreams.to_vec();

    if let Some(upstream) = postgres_mcp.reconcile().await? {
        upstreams.push(upstream);
    }

    Ok(upstreams)
}

async fn handle_call(
    clients: &McpClients,
    id: String,
    upstream: &str,
    tool_name: &str,
    arguments: Option<serde_json::Map<String, serde_json::Value>>,
) -> TunnelMessage {
    match call_tool(clients, upstream, tool_name, arguments).await {
        Ok(result) => TunnelMessage::CallToolResult { id, result },
        Err(error) => TunnelMessage::CallToolError { id, error },
    }
}
