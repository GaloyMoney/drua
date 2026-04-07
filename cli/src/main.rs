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
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install default CryptoProvider");

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
    sqlx::migrate!("../core/migrations").run(&pool).await?;

    let app_config = galoy_agents_core::AppConfig {
        toolsets: config.toolsets.clone(),
    };
    let sandbox_client = sandbox_client::SandboxClient::try_from_env(
        config.sandbox.namespace.clone(),
        config.sandbox.template_name.clone(),
    )
    .await
    .ok()
    .filter(|_| config.sandbox.enabled)
    .map(|client| {
        if let Some(ref p) = config.sandbox.persistence {
            client.with_persistence(sandbox_client::PersistenceConfig {
                size: p.size.clone(),
                storage_class: p.storage_class.clone(),
                mount_path: p.mount_path.clone(),
            })
        } else {
            client
        }
    });

    let app = galoy_agents_core::App::init(&pool, app_config, sandbox_client).await?;
    let auth_config = config.auth_config();
    let oauth_client = auth_config.oauth_client();
    let server_config = galoy_agents_web::server::ServerConfig {
        host: config.server.host.clone(),
        port: config.server.port,
        secure_cookies: config.server.secure_cookies,
    };

    let mcp_service = galoy_agents_mcp_gateway::McpGateway::service(app.clone());

    let app_state = galoy_agents_web::AppState::new(
        app,
        oauth_client,
        config.server.mcp_endpoint.clone(),
        auth_config.github_allowed_teams,
    );

    let router = galoy_agents_web::server::build_app(&server_config, &pool, app_state, mcp_service);

    let addr: std::net::SocketAddr =
        format!("{}:{}", config.server.host, config.server.port).parse()?;
    tracing::info!("Starting server on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router).await?;

    Ok(())
}
