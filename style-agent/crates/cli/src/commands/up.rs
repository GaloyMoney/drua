use crate::config::Config;

/// Start the HTTP MCP server.
pub async fn run(config: &Config) -> anyhow::Result<()> {
    let core_config = config.core_config();
    let bind_addr = config.bind_addr();
    style_agent_server::run_server_with_config(&core_config, &bind_addr).await
}
