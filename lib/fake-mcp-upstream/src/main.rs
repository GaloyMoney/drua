use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use axum::Router;
use clap::Parser;
use fake_mcp_upstream::FakeUpstream;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(about = "Tiny MCP server that serves fixture files as tools.")]
struct Cli {
    /// Directory containing fixture .json files.
    #[arg(long, default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures"))]
    fixtures_dir: PathBuf,

    /// Address to bind the HTTP server to.
    #[arg(long, default_value = "127.0.0.1:8765")]
    bind: SocketAddr,

    /// HTTP path to mount the MCP service at.
    #[arg(long, default_value = "/mcp")]
    mount: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let upstream = FakeUpstream::load(&cli.fixtures_dir)?;
    let count = upstream.fixture_count();
    let service = upstream.into_service();
    let app = Router::new().nest_service(&cli.mount, service);
    let listener = tokio::net::TcpListener::bind(cli.bind).await?;
    let local_addr = listener.local_addr()?;
    tracing::info!(
        "fake-mcp-upstream listening on http://{local_addr}{} ({count} fixtures from {})",
        cli.mount,
        cli.fixtures_dir.display()
    );
    axum::serve(listener, app).await?;
    Ok(())
}
