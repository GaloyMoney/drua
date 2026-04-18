//! Tunnel connector — runs in a target cluster and relays MCP tool calls
//! from galoy-agents to local MCP servers over an outbound WebSocket.
//!
//! # Usage
//!
//! ```text
//! tunnel-connector \
//!   --server-url wss://mcp.agents.galoy.io/tunnel/ws \
//!   --auth-token <bearer-token> \
//!   --deployment-id galoy-staging \
//!   --upstreams "kubernetes=http://k8s-mcp:8080/mcp,postgres=http://pg-mcp:8000/mcp"
//! ```
//!
//! The connector discovers tools from each upstream MCP server at startup,
//! connects to galoy-agents, registers the tool catalog, and enters a
//! relay loop. On disconnect it automatically reconnects.

use std::collections::HashMap;

use clap::Parser;
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
    about = "Outbound tunnel from a deployment cluster to galoy-agents"
)]
struct Cli {
    /// galoy-agents tunnel WebSocket URL
    #[arg(long, env = "TUNNEL_SERVER_URL")]
    server_url: String,

    /// Bearer token for authentication
    #[arg(long, env = "TUNNEL_AUTH_TOKEN")]
    auth_token: String,

    /// Deployment identifier (e.g. "galoy-staging")
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

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
    let upstreams = parse_upstreams(&cli.upstreams);

    if upstreams.is_empty() {
        anyhow::bail!("no upstreams configured — provide at least one name=url pair");
    }

    loop {
        match run_tunnel(&cli, &upstreams).await {
            Ok(()) => tracing::info!("tunnel closed cleanly"),
            Err(e) => tracing::error!(error = %e, "tunnel session failed"),
        }
        tracing::info!("reconnecting in 5 seconds...");
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

async fn run_tunnel(cli: &Cli, upstreams: &[UpstreamConfig]) -> anyhow::Result<()> {
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

    // ── 2. Connect WebSocket to galoy-agents ──────────────────────────────
    let parsed_url = url::Url::parse(&cli.server_url)?;
    let host = parsed_url.host_str().unwrap_or("localhost").to_string();

    let request = tungstenite::http::Request::builder()
        .uri(&cli.server_url)
        .header("Authorization", format!("Bearer {}", cli.auth_token))
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

    tracing::info!(server = %cli.server_url, "connected to galoy-agents");

    // ── 3. Send registration ──────────────────────────────────────────────
    let register_msg = TunnelMessage::Register {
        deployment_id: cli.deployment_id.clone(),
        toolsets: registrations,
    };
    let json = serde_json::to_string(&register_msg)?;
    ws_tx.send(tungstenite::Message::Text(json.into())).await?;
    tracing::info!("registration sent");

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
