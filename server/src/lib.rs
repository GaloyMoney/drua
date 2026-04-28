pub mod auth;
pub mod config;
pub mod graphql;
mod routes;
pub mod server;
mod templates;
pub mod tracing_init;
pub mod tunnel;
pub mod webhook;

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;

use drua_core as domain;

use domain::App;

use auth::config::{LoginMethod, OAuthClient};
use auth::sa_token::SaTokenValidator;
use auth::session_store::PgSessionStore;
use domain::code_assistant::CodeAssistant;

/// Parsed once at boot from `config.server.tunnel.deployments`.
pub type TunnelPublicKeys = Arc<HashMap<String, ed25519_dalek::VerifyingKey>>;

#[derive(Clone)]
pub struct AppState {
    pub app: App,
    pub oauth_client: OAuthClient,
    pub login: LoginMethod,
    pub mcp_endpoint: String,
    pub github_allowed_teams: Vec<String>,
    pub code_assistant: Option<CodeAssistant>,
    /// Exposed via the `appConfig` GraphQL query.
    pub app_config_yaml: graphql::Yaml,
    /// Present when running in-cluster.
    pub sa_token_validator: Option<SaTokenValidator>,
    pub session_store: PgSessionStore,
    pub tunnel_public_keys: TunnelPublicKeys,
    pub library_repo_url: Option<String>,
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: &sqlx::PgPool,
        app: App,
        oauth_client: OAuthClient,
        login: LoginMethod,
        mcp_endpoint: String,
        github_allowed_teams: Vec<String>,
        app_config_yaml: graphql::Yaml,
        tunnel_public_keys: TunnelPublicKeys,
    ) -> Self {
        let code_assistant = app.code_assistant().cloned();
        Self {
            app,
            oauth_client,
            login,
            mcp_endpoint,
            github_allowed_teams,
            code_assistant,
            app_config_yaml,
            sa_token_validator: None,
            session_store: PgSessionStore::new(pool),
            tunnel_public_keys,
            library_repo_url: None,
        }
    }

    pub fn with_sa_token_validator(mut self, validator: SaTokenValidator) -> Self {
        self.sa_token_validator = Some(validator);
        self
    }
}

pub fn router() -> Router<AppState> {
    routes::router()
        .merge(auth::auth_router())
        .merge(routes::api_router())
        .merge(graphql::router())
        .merge(webhook::router())
}

pub struct RunServerArgs {
    pub config_path: String,
    pub pg_con: String,
    pub github_client_secret: String,
    pub github_allowed_teams: String,
    pub anthropic_api_key: String,
    pub openai_api_key: String,
    pub config_overrides: Vec<String>,
}

pub fn dump_default_config() -> anyhow::Result<()> {
    let default_config = config::Config::default();
    let yaml_output = serde_yaml::to_string(&default_config)?;
    println!("{yaml_output}");
    Ok(())
}

pub async fn run_server(args: RunServerArgs) -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install default CryptoProvider");

    tracing_init::init_tracer(tracing_init::TracingConfig {
        service_name: "drua".to_string(),
    })?;

    let allowed_teams: Vec<String> = args
        .github_allowed_teams
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();

    let config = config::Config::try_new(
        &args.config_path,
        config::EnvSecrets {
            pg_con: args.pg_con,
            github_client_secret: args.github_client_secret,
            github_allowed_teams: allowed_teams,
            anthropic_api_key: args.anthropic_api_key,
            openai_api_key: args.openai_api_key,
        },
        &args.config_overrides,
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
            Some(domain::github_app::GitHubAppConfig {
                client_id: gh.client_id.clone(),
                installation_id: gh.installation_id.clone(),
                private_key_path: gh.private_key_path.clone(),
            })
        }
    });

    let app_config = domain::AppConfig {
        agents: config.agents.clone(),
        prompt_executor: config.prompt_executor_config(),
        toolsets: config.toolsets.clone(),
        encryption: Default::default(),
        sandbox: config.sandbox.clone(),
        github_app: github_app_config,
        library: config.library.clone(),
    };

    let app = domain::App::init(&pool, app_config).await?;
    let auth_config = config.auth_config();
    let oauth_client = auth_config.oauth_client();
    let server_config = server::ServerConfig {
        host: config.server.host.clone(),
        port: config.server.port,
        secure_cookies: config.server.secure_cookies,
    };

    let mcp_service = drua_mcp_gateway::McpGateway::service(app.clone());

    let app_config_yaml: graphql::Yaml = serde_yaml::to_string(&config)?.into();

    let tunnel_public_keys = tunnel::parse_configured_keys(&config.server.tunnel)?;
    if !tunnel_public_keys.is_empty() {
        tracing::info!(
            count = tunnel_public_keys.len(),
            "tunnel deployments registered"
        );
    }

    let mut app_state = AppState::new(
        &pool,
        app,
        oauth_client,
        auth_config.login,
        config.server.mcp_endpoint.clone(),
        auth_config.github_allowed_teams,
        app_config_yaml,
        std::sync::Arc::new(tunnel_public_keys),
    );
    app_state.library_repo_url = config.library.repo_url.clone();

    if let Some(validator) = auth::sa_token::SaTokenValidator::try_from_env("drua-mcp").await {
        tracing::info!("SA token validator initialized (in-cluster)");
        app_state = app_state.with_sa_token_validator(validator);
    }

    let router = server::build_app(&server_config, app_state, mcp_service);

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
