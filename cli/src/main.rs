mod config;
mod tracing_init;

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

    /// Anthropic API key for the light agent runtime.
    #[arg(long, env = "ANTHROPIC_API_KEY", default_value = "")]
    anthropic_api_key: String,

    /// OpenAI API key for the light agent runtime (takes priority over Anthropic).
    #[arg(long, env = "OPENAI_API_KEY", default_value = "")]
    openai_api_key: String,

    /// OpenAI model to use (default: gpt-4.1).
    #[arg(long, env = "OPENAI_MODEL", default_value = "")]
    openai_model: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install default CryptoProvider");

    tracing_init::init_tracer(tracing_init::TracingConfig {
        service_name: "galoy-agents".to_string(),
    })?;

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
            anthropic_api_key: cli.anthropic_api_key,
            openai_api_key: cli.openai_api_key,
            openai_model: cli.openai_model,
        },
    )?;

    let pool = sqlx::PgPool::connect(&config.db.pg_con).await?;
    sqlx::migrate!("../core/migrations").run(&pool).await?;

    let github_app_config = config.github_app.as_ref().and_then(|gh| {
        if gh.client_id.is_empty()
            || gh.installation_id.is_empty()
            || gh.private_key_path.is_empty()
        {
            None
        } else {
            Some(galoy_agents_core::github_app::GitHubAppConfig {
                client_id: gh.client_id.clone(),
                installation_id: gh.installation_id.clone(),
                private_key_path: gh.private_key_path.clone(),
            })
        }
    });

    let app_config = galoy_agents_core::AppConfig {
        agents: galoy_agents_core::agent::AgentConfig {
            sandbox: galoy_agents_core::agent::config::SandboxClientConfig {
                enabled: config.sandbox.enabled,
                namespace: config.sandbox.namespace.clone(),
                template_name: config.sandbox.template_name.clone(),
                persistence: config.sandbox.persistence.as_ref().map(|p| {
                    galoy_agents_core::agent::config::PersistenceConfig {
                        size: p.size.clone(),
                        storage_class: p.storage_class.clone(),
                        mount_path: p.mount_path.clone(),
                    }
                }),
            },
            light: galoy_agents_core::agent::config::LightRuntimeConfig {
                api_key: config.anthropic_api_key.clone(),
                openai_api_key: config.openai_api_key.clone(),
                openai_model: config.openai_model.clone(),
            },
        },
        toolsets: config.toolsets.clone(),
        encryption: Default::default(),
        github_app: github_app_config,
    };

    let app = galoy_agents_core::App::init(&pool, app_config).await?;
    let auth_config = config.auth_config();
    let oauth_client = auth_config.oauth_client();
    let server_config = galoy_agents_web::server::ServerConfig {
        host: config.server.host.clone(),
        port: config.server.port,
        secure_cookies: config.server.secure_cookies,
    };

    let mcp_service = galoy_agents_mcp_gateway::McpGateway::service(app.clone());

    let mut app_state = galoy_agents_web::AppState::new(
        app,
        oauth_client,
        config.server.mcp_endpoint.clone(),
        auth_config.github_allowed_teams,
    );

    // Initialize SA token validator for sandbox pod authentication (in-cluster only)
    if let Some(validator) =
        galoy_agents_web::auth::sa_token::SaTokenValidator::try_from_env("galoy-agents-mcp").await
    {
        tracing::info!("SA token validator initialized (in-cluster)");
        app_state = app_state.with_sa_token_validator(validator);
    }

    let router = galoy_agents_web::server::build_app(&server_config, &pool, app_state, mcp_service);

    let addr: std::net::SocketAddr =
        format!("{}:{}", config.server.host, config.server.port).parse()?;
    tracing::info!("Starting server on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router).await?;

    if let Err(e) = tracing_init::shutdown_tracer() {
        eprintln!("Error shutting down tracer: {e}");
    }

    Ok(())
}
