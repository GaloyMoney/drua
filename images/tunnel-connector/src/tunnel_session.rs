use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite;

use crate::{
    cli::Cli,
    mcp_upstream::{
        call_tool, discover_upstream, registration_fingerprint, tool_catalog_changed, McpClients,
        UpstreamConfig,
    },
    postgres_mcp::{PostgresMcpController, PostgresMcpHandler, PostgresSourceDiscoverer},
    tunnel_auth::{load_signing_key, sign_handshake},
    tunnel_protocol::TunnelMessage,
};

pub(crate) async fn run_tunnel<H, D>(
    cli: &Cli,
    static_upstreams: &[UpstreamConfig],
    postgres_mcp: &PostgresMcpController<H, D>,
    backoff: &mut std::time::Duration,
) -> anyhow::Result<()>
where
    H: PostgresMcpHandler,
    D: PostgresSourceDiscoverer,
{
    let upstreams = build_upstreams(static_upstreams, postgres_mcp).await?;
    if upstreams.is_empty() {
        anyhow::bail!("all configured MCP upstreams are unavailable or disabled");
    }

    let mut mcp_clients = McpClients::new();
    let mut registrations = Vec::new();
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

    let registration_fingerprint = registration_fingerprint(&registrations)?;
    let register_msg = TunnelMessage::Register {
        deployment_id: cli.deployment_id.clone(),
        toolsets: registrations,
    };
    let json = serde_json::to_string(&register_msg)?;
    ws_tx.send(tungstenite::Message::Text(json.into())).await?;
    tracing::info!("registration sent");

    *backoff = crate::INITIAL_BACKOFF;

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

async fn build_upstreams<H, D>(
    static_upstreams: &[UpstreamConfig],
    postgres_mcp: &PostgresMcpController<H, D>,
) -> anyhow::Result<Vec<UpstreamConfig>>
where
    H: PostgresMcpHandler,
    D: PostgresSourceDiscoverer,
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
