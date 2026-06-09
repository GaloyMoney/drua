//! Tunnel connector — runs in a target cluster, discovers MCP tools locally,
//! dials out to drua over WSS with an Ed25519-signed handshake, and relays tool
//! calls. Reconnects with exponential backoff + jitter.

use std::sync::Arc;

use clap::Parser;
use mcp_upstream::parse_upstreams;
use postgres_mcp::{PostgresMcpConfig, PostgresMcpController};
use postgres_mcp_kubernetes::{resolve_namespace, KubernetesPostgresMcpHandler};
use postgres_mcp_postgres::SqlxPostgresSourceDiscoverer;
use tunnel_session::run_tunnel;

mod cli;
mod mcp_upstream;
mod postgres_mcp;
mod postgres_mcp_kubernetes;
mod postgres_mcp_postgres;
#[cfg(test)]
mod postgres_mcp_recreate_test;
mod tunnel_auth;
mod tunnel_protocol;
#[cfg(test)]
mod tunnel_registration_test;
mod tunnel_session;

type ManagedPostgresMcpController =
    PostgresMcpController<KubernetesPostgresMcpHandler, SqlxPostgresSourceDiscoverer>;

pub(crate) const INITIAL_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);
const MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(60);
const JITTER_MS_MAX: u64 = 1_000;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cli = cli::Cli::parse();
    let static_upstreams = parse_upstreams(&cli.upstreams);
    let postgres_mcp = build_postgres_mcp(&cli).await?;

    let mut backoff = INITIAL_BACKOFF;
    loop {
        match run_tunnel(&cli, &static_upstreams, postgres_mcp.as_ref(), &mut backoff).await {
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

async fn build_postgres_mcp(cli: &cli::Cli) -> anyhow::Result<Arc<ManagedPostgresMcpController>> {
    let config = PostgresMcpConfig {
        namespace: resolve_namespace(cli.postgres_mcp_namespace.clone())?,
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
        runtime_seed_dsn: cli.postgres_mcp_runtime_dsn.clone(),
        datawarehouse_seed_dsn: cli
            .postgres_mcp_datawarehouse_dsn
            .as_deref()
            .map(str::trim)
            .filter(|dsn| !dsn.is_empty())
            .map(str::to_string),
    };

    let handler = KubernetesPostgresMcpHandler::try_default().await?;
    let discoverer = SqlxPostgresSourceDiscoverer;

    Ok(Arc::new(PostgresMcpController::try_new(
        config, handler, discoverer,
    )?))
}
