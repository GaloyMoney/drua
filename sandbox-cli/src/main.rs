use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use crossterm::terminal;
use futures::{SinkExt, StreamExt};
use tokio::io::AsyncWriteExt;
use tokio_tungstenite::tungstenite;

/// Wire protocol frame types — must match the server.
mod ws_proto {
    pub const STDIN: u8 = 0x01;
    pub const STDOUT: u8 = 0x02;
    #[allow(dead_code)]
    pub const STDERR: u8 = 0x03;
    pub const RESIZE: u8 = 0x04;
    pub const EXIT: u8 = 0xFF;
}

#[derive(Parser)]
#[command(name = "sandbox-cli", about = "Manage Agent Sandbox pods")]
struct Cli {
    #[command(subcommand)]
    command: Command,

    #[command(flatten)]
    global: GlobalArgs,
}

#[derive(Parser, Clone)]
struct GlobalArgs {
    /// Base URL of the galoy-agents server.
    #[arg(
        long,
        env = "GALOY_AGENTS_URL",
        default_value = "https://dashboard.agents.galoy.io"
    )]
    server_url: String,

    /// Bearer token for authentication.
    #[arg(long, env = "GALOY_AGENTS_TOKEN")]
    token: String,
}

#[derive(Subcommand)]
enum Command {
    /// Create a sandbox claim and optionally wait for it to be ready.
    Create {
        /// Name for the sandbox claim.
        name: String,

        /// Don't wait for the sandbox to become ready.
        #[arg(long)]
        no_wait: bool,

        /// Timeout in seconds when waiting for readiness.
        #[arg(long, default_value = "120")]
        timeout: u64,
    },

    /// Exec into a running sandbox pod.
    Exec {
        /// Name of the sandbox claim.
        name: String,

        /// Command to run inside the sandbox.
        #[arg(long, default_value = "sh")]
        cmd: String,
    },

    /// Delete a sandbox claim.
    Delete {
        /// Name of the sandbox claim.
        name: String,
    },

    /// List all sandbox claims.
    List,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("Failed to create HTTP client")?;

    let base = cli.global.server_url.trim_end_matches('/').to_string();
    let token = cli.global.token.clone();

    match cli.command {
        Command::Create {
            name,
            no_wait,
            timeout,
        } => cmd_create(&http, &base, &token, &name, no_wait, timeout).await,
        Command::Exec { name, cmd } => cmd_exec(&base, &token, &name, &cmd).await,
        Command::Delete { name } => cmd_delete(&http, &base, &token, &name).await,
        Command::List => cmd_list(&http, &base, &token).await,
    }
}

#[derive(serde::Deserialize, Debug)]
struct SandboxSummary {
    name: String,
    sandbox_name: Option<String>,
    phase: String,
    ready: bool,
}

async fn cmd_create(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    name: &str,
    no_wait: bool,
    timeout_secs: u64,
) -> anyhow::Result<()> {
    eprintln!("Creating sandbox claim '{name}' ...");

    let resp = http
        .post(format!("{base}/api/v1/sandboxes"))
        .bearer_auth(token)
        .json(&serde_json::json!({ "name": name }))
        .send()
        .await
        .context("Failed to reach server")?;

    if !resp.status().is_success() {
        anyhow::bail!("Create failed: HTTP {}", resp.status());
    }

    if no_wait {
        eprintln!("Claim created (not waiting for readiness).");
        return Ok(());
    }

    eprintln!("Waiting up to {timeout_secs}s for sandbox to become ready ...");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);

    loop {
        let summary = get_sandbox(http, base, token, name).await?;
        if summary.ready {
            eprintln!(
                "Sandbox ready: pod={}",
                summary.sandbox_name.as_deref().unwrap_or("?")
            );
            return Ok(());
        }

        if tokio::time::Instant::now() >= deadline {
            eprintln!("Timed out after {timeout_secs}s — sandbox not yet ready.");
            eprintln!("Use `sandbox-cli list` to check status later.");
            std::process::exit(1);
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn get_sandbox(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    name: &str,
) -> anyhow::Result<SandboxSummary> {
    let resp = http
        .get(format!("{base}/api/v1/sandboxes/{name}"))
        .bearer_auth(token)
        .send()
        .await
        .context("Failed to reach server")?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("Sandbox '{name}' not found");
    }
    if !resp.status().is_success() {
        anyhow::bail!("Get sandbox failed: HTTP {}", resp.status());
    }

    resp.json::<SandboxSummary>()
        .await
        .context("Failed to parse sandbox response")
}

async fn cmd_exec(base: &str, token: &str, name: &str, cmd: &str) -> anyhow::Result<()> {
    eprintln!("Connecting to sandbox '{name}' (command: {cmd}) ...");

    // Build WebSocket URL
    let ws_base = base
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    let ws_url = format!("{ws_base}/api/v1/sandboxes/{name}/exec?cmd={cmd}");

    let url = url::Url::parse(&ws_url).context("Invalid WebSocket URL")?;

    // Build request with Authorization header
    let request = tungstenite::http::Request::builder()
        .uri(ws_url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Host", url.host_str().unwrap_or("localhost"))
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header(
            "Sec-WebSocket-Key",
            tungstenite::handshake::client::generate_key(),
        )
        .body(())
        .context("Failed to build WebSocket request")?;

    let (ws_stream, _) = tokio_tungstenite::connect_async(request)
        .await
        .context("WebSocket connection failed")?;

    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdin());

    if is_tty {
        eprintln!("Connected. Press Ctrl-C to exit.\r");
    } else {
        eprintln!("Connected (piped mode).");
    }

    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    // Set terminal to raw mode (only for interactive TTY)
    let _raw_guard = if is_tty {
        Some(RawModeGuard::enable()?)
    } else {
        None
    };

    // Send initial terminal size (only for interactive TTY)
    if is_tty {
        if let Ok((cols, rows)) = terminal::size() {
            let mut frame = Vec::with_capacity(5);
            frame.push(ws_proto::RESIZE);
            frame.extend_from_slice(&cols.to_be_bytes());
            frame.extend_from_slice(&rows.to_be_bytes());
            let _ = ws_sender
                .send(tungstenite::Message::Binary(frame.into()))
                .await;
        }
    }

    // Stdin reader channel
    let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let stdin = std::io::stdin();
        let mut handle = stdin.lock();
        let mut buf = [0u8; 1024];
        loop {
            match handle.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if stdin_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut exit_code: Option<u8> = None;

    loop {
        tokio::select! {
            // Server → local stdout
            msg = ws_receiver.next() => {
                match msg {
                    Some(Ok(tungstenite::Message::Binary(data))) => {
                        if data.is_empty() {
                            continue;
                        }
                        match data[0] {
                            ws_proto::STDOUT | ws_proto::STDERR => {
                                let mut stdout = tokio::io::stdout();
                                let _ = stdout.write_all(&data[1..]).await;
                                let _ = stdout.flush().await;
                            }
                            ws_proto::EXIT => {
                                exit_code = data.get(1).copied();
                                break;
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(tungstenite::Message::Close(_))) | None => break,
                    Some(Err(e)) => {
                        eprintln!("\r\nConnection error: {e}");
                        break;
                    }
                    _ => {}
                }
            }
            // Local stdin → server
            bytes = stdin_rx.recv() => {
                match bytes {
                    Some(data) => {
                        let mut frame = Vec::with_capacity(1 + data.len());
                        frame.push(ws_proto::STDIN);
                        frame.extend_from_slice(&data);
                        if ws_sender.send(tungstenite::Message::Binary(frame.into())).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
    }

    drop(_raw_guard);

    if let Some(code) = exit_code {
        if code != 0 {
            std::process::exit(code as i32);
        }
    }

    Ok(())
}

async fn cmd_delete(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    name: &str,
) -> anyhow::Result<()> {
    eprintln!("Deleting sandbox claim '{name}' ...");

    let resp = http
        .delete(format!("{base}/api/v1/sandboxes/{name}"))
        .bearer_auth(token)
        .send()
        .await
        .context("Failed to reach server")?;

    if !resp.status().is_success() {
        anyhow::bail!("Delete failed: HTTP {}", resp.status());
    }

    eprintln!("Deleted.");
    Ok(())
}

async fn cmd_list(http: &reqwest::Client, base: &str, token: &str) -> anyhow::Result<()> {
    let resp = http
        .get(format!("{base}/api/v1/sandboxes"))
        .bearer_auth(token)
        .send()
        .await
        .context("Failed to reach server")?;

    if !resp.status().is_success() {
        anyhow::bail!("List failed: HTTP {}", resp.status());
    }

    let claims: Vec<SandboxSummary> = resp.json().await.context("Failed to parse sandbox list")?;

    if claims.is_empty() {
        eprintln!("No sandbox claims found.");
        return Ok(());
    }

    let header = format!(
        "{:<30} {:<30} {:<15} {}",
        "CLAIM", "SANDBOX", "PHASE", "READY"
    );
    println!("{header}");
    for c in &claims {
        println!(
            "{:<30} {:<30} {:<15} {}",
            c.name,
            c.sandbox_name.as_deref().unwrap_or("-"),
            c.phase,
            if c.ready { "yes" } else { "no" },
        );
    }

    Ok(())
}

/// RAII guard that restores the terminal from raw mode on drop.
struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> anyhow::Result<Self> {
        terminal::enable_raw_mode().context("Failed to enable raw mode")?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}
