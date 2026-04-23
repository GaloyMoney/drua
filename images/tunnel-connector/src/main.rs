//! Tunnel connector — runs in a target cluster, discovers MCP tools
//! locally, dials out to drua over WSS with an Ed25519-signed handshake,
//! relays tool calls. Reconnects with exponential backoff + jitter.

use std::collections::HashMap;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use clap::Parser;
use ed25519_dalek::{pkcs8::DecodePrivateKey, Signer, SigningKey};
use futures::{SinkExt, StreamExt};
use rmcp::{
    model::CallToolRequestParams,
    service::RunningService,
    transport::streamable_http_client::{
        StreamableHttpClientTransportConfig, StreamableHttpClientWorker,
    },
    RoleClient, ServiceExt,
};
use serde::{Deserialize, Serialize};
use tokio_tungstenite::tungstenite;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RegisteredToolSet {
    name: String,
    prefix: String,
    category: String,
    category_description: String,
    tools: Vec<serde_json::Value>,
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

    /// Comma-separated upstream MCP servers: name=url[,name=url,...]
    #[arg(long, env = "TUNNEL_UPSTREAMS")]
    upstreams: String,
}

struct UpstreamConfig {
    name: String,
    url: String,
}

fn parse_upstreams(raw: &str) -> Vec<UpstreamConfig> {
    raw.split(',')
        .filter_map(|pair| {
            let (name, url) = pair.split_once('=')?;
            Some(UpstreamConfig {
                name: name.trim().to_string(),
                url: url.trim().to_string(),
            })
        })
        .collect()
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
    let upstreams = parse_upstreams(&cli.upstreams);

    if upstreams.is_empty() {
        anyhow::bail!("no upstreams configured — provide at least one name=url pair");
    }

    let mut backoff = INITIAL_BACKOFF;
    loop {
        // `run_tunnel` resets `backoff` to `INITIAL_BACKOFF` once it has
        // successfully sent the Register frame. That way a long-lived
        // session that eventually errors out doesn't start its *next*
        // reconnect from a stale high backoff.
        match run_tunnel(&cli, &upstreams, &mut backoff).await {
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

async fn run_tunnel(
    cli: &Cli,
    upstreams: &[UpstreamConfig],
    backoff: &mut std::time::Duration,
) -> anyhow::Result<()> {
    // ── 1. Discover tools from local MCP servers ──────────────────────────
    let mut mcp_clients: HashMap<String, RunningService<RoleClient, ()>> = HashMap::new();
    let mut registrations: Vec<RegisteredToolSet> = Vec::new();

    for upstream in upstreams {
        tracing::info!(name = %upstream.name, url = %upstream.url, "connecting to local MCP server");

        let config = StreamableHttpClientTransportConfig::with_uri(upstream.url.as_str());
        let worker = StreamableHttpClientWorker::new(reqwest::Client::new(), config);
        let client: RunningService<RoleClient, ()> = ().serve(worker).await?;

        let tools: Vec<serde_json::Value> = client
            .list_all_tools()
            .await?
            .into_iter()
            .filter_map(|t| serde_json::to_value(t).ok())
            .collect();

        tracing::info!(name = %upstream.name, tools = tools.len(), "discovered tools");

        registrations.push(RegisteredToolSet {
            name: upstream.name.clone(),
            prefix: upstream.name.clone(),
            category: "deployment".to_string(),
            category_description: format!("{} deployment", cli.deployment_id),
            tools,
        });

        mcp_clients.insert(upstream.name.clone(), client);
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
    while let Some(msg) = ws_rx.next().await {
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

    Ok(())
}

async fn handle_call(
    clients: &HashMap<String, RunningService<RoleClient, ()>>,
    id: String,
    upstream: &str,
    tool_name: &str,
    arguments: Option<serde_json::Map<String, serde_json::Value>>,
) -> TunnelMessage {
    let client = match clients.get(upstream) {
        Some(c) => c,
        None => {
            return TunnelMessage::CallToolError {
                id,
                error: format!("unknown upstream: {upstream}"),
            }
        }
    };

    let mut params = CallToolRequestParams::new(tool_name.to_string());
    if let Some(args) = arguments {
        params = params.with_arguments(args);
    }

    match client.peer().call_tool(params).await {
        Ok(result) => match serde_json::to_value(&result) {
            Ok(value) => TunnelMessage::CallToolResult { id, result: value },
            Err(e) => TunnelMessage::CallToolError {
                id,
                error: format!("serialize result: {e}"),
            },
        },
        Err(e) => TunnelMessage::CallToolError {
            id,
            error: e.to_string(),
        },
    }
}
