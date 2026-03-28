use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use crossterm::terminal;
use sandbox_client::{SandboxClient, SandboxError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
    /// Kubernetes namespace for sandbox resources.
    #[arg(long, env = "SANDBOX_NAMESPACE", default_value = "galoy-agents")]
    namespace: String,

    /// Name of the SandboxTemplate to use.
    #[arg(long, env = "SANDBOX_TEMPLATE", default_value = "agent-sandbox")]
    template: String,
}

#[derive(Subcommand)]
enum Command {
    /// Create a sandbox claim and wait for it to be ready.
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

    /// List all sandbox claims in the namespace.
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
    let client = SandboxClient::try_from_env(cli.global.namespace, cli.global.template)
        .await
        .context("Failed to create Kubernetes client")?;

    match cli.command {
        Command::Create {
            name,
            no_wait,
            timeout,
        } => cmd_create(&client, &name, no_wait, timeout).await,
        Command::Exec { name, cmd } => cmd_exec(&client, &name, &cmd).await,
        Command::Delete { name } => cmd_delete(&client, &name).await,
        Command::List => cmd_list(&client).await,
    }
}

async fn cmd_create(
    client: &SandboxClient,
    name: &str,
    no_wait: bool,
    timeout_secs: u64,
) -> anyhow::Result<()> {
    eprintln!("Creating sandbox claim '{name}' ...");
    client
        .create_claim(name)
        .await
        .context("Failed to create sandbox claim")?;

    if no_wait {
        eprintln!("Claim created (not waiting for readiness).");
        return Ok(());
    }

    eprintln!("Waiting up to {timeout_secs}s for sandbox to become ready ...");
    match client
        .wait_until_ready(name, Duration::from_secs(timeout_secs))
        .await
    {
        Ok(summary) => {
            eprintln!(
                "Sandbox ready: pod={}",
                summary.sandbox_name.as_deref().unwrap_or("?")
            );
        }
        Err(SandboxError::Timeout(_)) => {
            eprintln!("Timed out after {timeout_secs}s — sandbox not yet ready.");
            eprintln!("Use `sandbox-cli list` to check status later.");
            std::process::exit(1);
        }
        Err(e) => return Err(e.into()),
    }

    Ok(())
}

async fn cmd_exec(client: &SandboxClient, name: &str, cmd: &str) -> anyhow::Result<()> {
    let command: Vec<String> = cmd.split_whitespace().map(String::from).collect();

    eprintln!("Attaching to sandbox '{name}' (command: {cmd}) ...");

    let mut process = client
        .exec_in_sandbox(name, command)
        .await
        .context("Failed to exec into sandbox")?;

    // Take ownership of the streams before enabling raw mode
    let mut remote_stdin = process.stdin().unwrap();
    let mut remote_stdout = process.stdout().unwrap();

    // Set terminal to raw mode for interactive use
    let _raw_guard = RawModeGuard::enable()?;

    // Channel for stdin bytes from the blocking reader
    let (stdin_tx, mut stdin_rx) =
        tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(64);

    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let stdin = std::io::stdin();
        let mut handle = stdin.lock();
        let mut buf = [0u8; 1024];

        loop {
            match handle.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if stdin_tx.blocking_send(Ok(buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = stdin_tx.blocking_send(Err(e));
                    break;
                }
            }
        }
    });

    let mut stdout_buf = [0u8; 4096];

    loop {
        tokio::select! {
            // Remote stdout → local stdout
            result = remote_stdout.read(&mut stdout_buf) => {
                match result {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut stdout = tokio::io::stdout();
                        stdout.write_all(&stdout_buf[..n]).await?;
                        stdout.flush().await?;
                    }
                    Err(e) => {
                        eprintln!("\r\nStream error: {e}");
                        break;
                    }
                }
            }
            // Local stdin → remote stdin
            chunk = stdin_rx.recv() => {
                match chunk {
                    Some(Ok(bytes)) => {
                        remote_stdin.write_all(&bytes).await?;
                    }
                    Some(Err(e)) => {
                        eprintln!("\r\nStdin error: {e}");
                        break;
                    }
                    None => break,
                }
            }
        }
    }

    // Drop the raw mode guard to restore the terminal
    drop(_raw_guard);

    process.join().await?;

    Ok(())
}

async fn cmd_delete(client: &SandboxClient, name: &str) -> anyhow::Result<()> {
    eprintln!("Deleting sandbox claim '{name}' ...");
    client
        .delete_claim(name)
        .await
        .context("Failed to delete sandbox claim")?;
    eprintln!("Deleted.");
    Ok(())
}

async fn cmd_list(client: &SandboxClient) -> anyhow::Result<()> {
    let claims = client
        .list_claims()
        .await
        .context("Failed to list sandbox claims")?;

    if claims.is_empty() {
        eprintln!(
            "No sandbox claims found in namespace '{}'.",
            client.namespace()
        );
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
