mod config;

use clap::Parser;

use config::{Config, EnvSecrets};

#[derive(Parser)]
#[command(name = "galoy-agents", about = "Galoy Agents CLI")]
struct Cli {
    /// Path to config file
    #[arg(long, env = "GALOY_AGENTS_CONFIG", default_value = "galoy-agents.yml")]
    config: String,

    /// PostgreSQL connection URL
    #[arg(long, env = "PG_CON")]
    pg_con: String,

    /// GitHub OAuth client secret
    #[arg(long, env = "GITHUB_CLIENT_SECRET")]
    github_client_secret: String,

    /// Comma-separated list of allowed GitHub teams (org/team-slug format).
    /// If empty, all GitHub users can log in.
    #[arg(long, env = "GITHUB_ALLOWED_TEAMS", default_value = "")]
    github_allowed_teams: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    let allowed_teams: Vec<String> = cli
        .github_allowed_teams
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();

    let config = Config::try_new(
        &cli.config,
        EnvSecrets {
            pg_con: cli.pg_con,
            github_client_secret: cli.github_client_secret,
            github_allowed_teams: allowed_teams,
        },
    )?;

    let pool = sqlx::PgPool::connect(&config.db.pg_con).await?;
    sqlx::migrate!("../domain/migrations").run(&pool).await?;

    let app = galoy_agents_domain::App::new(&pool);
    let auth_config = config.auth_config();
    let oauth_client = auth_config.oauth_client();
    let app_state = galoy_agents_web::AppState::new(
        app,
        oauth_client,
        config.server.mcp_endpoint.clone(),
        auth_config.github_allowed_teams,
    );

    let server_config = galoy_agents_web::server::ServerConfig {
        host: config.server.host.clone(),
        port: config.server.port,
        secure_cookies: config.server.secure_cookies,
    };

    let pg_logger = std::sync::Arc::new(galoy_agents_mcp_gateway::PgRequestLogger::new(&pool));
    let mcp_service = galoy_agents_mcp_gateway::McpGateway::service_with_logger(
        app_state.app.clone(),
        &config.style_agent,
        pg_logger,
    )?;

    let router = galoy_agents_web::server::build_app(&server_config, &pool, app_state, mcp_service);

    let addr: std::net::SocketAddr =
        format!("{}:{}", config.server.host, config.server.port).parse()?;
    tracing::info!("Starting server on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router).await?;

    Ok(())
}
