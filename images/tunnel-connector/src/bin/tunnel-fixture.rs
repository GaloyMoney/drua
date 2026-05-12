use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use clap::Parser;
use ed25519_dalek::{pkcs8::DecodePrivateKey, Signer, SigningKey};
use futures::{SinkExt, StreamExt};
use rmcp::model::{CallToolResult, Content, JsonObject, Tool};
use serde::{Deserialize, Serialize};
use tokio_tungstenite::tungstenite;

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

#[derive(Parser)]
struct Cli {
    /// Comma-separated drua tunnel WebSocket URLs. The fixture starts with
    /// the first URL and rotates after each disconnect to model LB failover.
    #[arg(long, env = "TUNNEL_SERVER_URLS")]
    server_urls: String,

    #[arg(long, env = "TUNNEL_PRIVATE_KEY_FILE")]
    private_key_file: std::path::PathBuf,

    #[arg(long, env = "TUNNEL_DEPLOYMENT_ID")]
    deployment_id: String,

    /// Touch this file after every successful Register frame.
    #[arg(long)]
    ready_file: Option<std::path::PathBuf>,

    #[arg(long, default_value_t = 100)]
    reconnect_delay_ms: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
    let urls: Vec<String> = cli
        .server_urls
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if urls.is_empty() {
        anyhow::bail!("no server URLs configured");
    }

    let signing_key = load_signing_key(&cli.private_key_file)?;
    let mut index = 0usize;
    loop {
        let url = &urls[index % urls.len()];
        if let Err(e) = run_session(&cli, &signing_key, url).await {
            tracing::warn!(
                deployment_id = %cli.deployment_id,
                server_url = %url,
                error = %e,
                "tunnel fixture session ended"
            );
        }
        index = index.wrapping_add(1);
        tokio::time::sleep(std::time::Duration::from_millis(cli.reconnect_delay_ms)).await;
    }
}

fn load_signing_key(path: &std::path::Path) -> anyhow::Result<SigningKey> {
    let pem = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading private key from {}: {e}", path.display()))?;
    SigningKey::from_pkcs8_pem(&pem)
        .map_err(|e| anyhow::anyhow!("private key is not PKCS#8 Ed25519 PEM: {e}"))
}

fn sign_handshake(deployment_id: &str, signing_key: &SigningKey) -> String {
    let ts_ms = chrono::Utc::now().timestamp_millis();
    let payload = format!("{deployment_id}|{ts_ms}");
    let sig = signing_key.sign(payload.as_bytes());
    let sig_b64 = URL_SAFE_NO_PAD.encode(sig.to_bytes());
    format!("Tunnel {deployment_id}:{ts_ms}:{sig_b64}")
}

async fn run_session(cli: &Cli, signing_key: &SigningKey, url: &str) -> anyhow::Result<()> {
    let parsed_url = url::Url::parse(url)?;
    let host = parsed_url.host_str().unwrap_or("localhost").to_string();
    let authorization = sign_handshake(&cli.deployment_id, signing_key);

    let request = tungstenite::http::Request::builder()
        .uri(url)
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

    let (ws_stream, _) = tokio_tungstenite::connect_async(request).await?;
    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    let register_msg = TunnelMessage::Register {
        deployment_id: cli.deployment_id.clone(),
        toolsets: vec![registered_echo_toolset(&cli.deployment_id)?],
    };
    ws_tx
        .send(tungstenite::Message::Text(
            serde_json::to_string(&register_msg)?.into(),
        ))
        .await?;

    if let Some(path) = &cli.ready_file {
        std::fs::write(path, format!("{url}\n"))?;
    }

    while let Some(msg) = ws_rx.next().await {
        let msg = msg?;
        match msg {
            tungstenite::Message::Text(text) => {
                let response = handle_text(&cli.deployment_id, url, &text)?;
                ws_tx
                    .send(tungstenite::Message::Text(
                        serde_json::to_string(&response)?.into(),
                    ))
                    .await?;
            }
            tungstenite::Message::Ping(data) => {
                ws_tx.send(tungstenite::Message::Pong(data)).await?;
            }
            tungstenite::Message::Close(_) => break,
            _ => {}
        }
    }

    Ok(())
}

fn registered_echo_toolset(deployment_id: &str) -> anyhow::Result<RegisteredToolSet> {
    let mut schema = JsonObject::new();
    schema.insert("type".into(), "object".into());
    schema.insert(
        "properties".into(),
        serde_json::json!({
            "probe": { "type": "string" }
        }),
    );

    let tool = Tool::new(
        "echo".to_string(),
        format!("Echoes the HA tunnel fixture identity for {deployment_id}."),
        schema,
    );

    Ok(RegisteredToolSet {
        name: "echo".to_string(),
        prefix: "echo".to_string(),
        category: "deployment".to_string(),
        category_description: format!("{deployment_id} fixture deployment"),
        tools: vec![serde_json::to_value(tool)?],
    })
}

fn handle_text(deployment_id: &str, server_url: &str, text: &str) -> anyhow::Result<TunnelMessage> {
    let msg: TunnelMessage = serde_json::from_str(text)?;
    match msg {
        TunnelMessage::CallTool {
            id,
            upstream,
            tool_name,
            arguments,
        } => {
            if upstream != "echo" || tool_name != "echo" {
                return Ok(TunnelMessage::CallToolError {
                    id,
                    error: format!("unknown fixture tool {upstream}.{tool_name}"),
                });
            }

            let probe = arguments
                .as_ref()
                .and_then(|m| m.get("probe"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let result = CallToolResult::success(vec![Content::text(format!(
                "deployment={deployment_id};probe={probe};served_by={server_url}"
            ))]);
            Ok(TunnelMessage::CallToolResult {
                id,
                result: serde_json::to_value(result)?,
            })
        }
        _ => Ok(TunnelMessage::CallToolError {
            id: "unknown".to_string(),
            error: "unexpected message".to_string(),
        }),
    }
}
