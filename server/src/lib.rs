pub mod auth;
pub mod config;
pub mod git_proxy;
pub mod graphql;
mod internal_routes;
mod routes;
pub mod server;
mod templates;
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

// Dummy CI trigger: 2026-05-05.
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
    pub dev_mode_agent_tokens: bool,
    /// Model ids from `providers[].models`, in config order; populates the
    /// agent model-chain dropdowns.
    pub model_options: Vec<String>,
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
            dev_mode_agent_tokens: false,
            model_options: Vec::new(),
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
        .merge(git_proxy::router())
        .merge(internal_routes::internal_router())
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

    drua_tracing_utils::init_tracer(drua_tracing_utils::TracingConfig {
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

    // Size the pool explicitly: the default `PgPool::connect` caps at 10, and
    // with N replicas the cluster-wide draw is `max_connections * N` — keep
    // that under the instance's `max_connections` (CloudSQL flag) or replicas
    // starve each other into `pool timed out`.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(config.db.max_connections)
        .connect(&config.db.pg_con)
        .await?;
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

    let pod_ip = std::env::var("POD_IP").ok().filter(|s| !s.is_empty());
    let self_pod_addr = pod_ip.map(|ip| format!("{}:{}", ip, config.server.port));
    let internal_secret = std::env::var("DRUA_TUNNEL_INTERNAL_SECRET")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| config.server.tunnel.internal_secret.clone());
    let tunnel_runtime = drua_tunnel::TunnelRuntimeConfig {
        self_pod_addr: self_pod_addr.clone(),
        heartbeat_secs: config.server.tunnel.heartbeat_secs,
        expires_after_secs: config.server.tunnel.expires_after_secs,
        reaper_interval_secs: config.server.tunnel.reaper_interval_secs,
        internal_secret,
    };
    if let Some(addr) = self_pod_addr.as_deref() {
        tracing::info!(self_pod_addr = %addr, "tunnel HA mode enabled");
    } else {
        tracing::info!("tunnel running in single-replica mode (POD_IP unset)");
    }

    let app_config = domain::AppConfig {
        agents: config.agents.clone(),
        prompt_executor: config.prompt_executor_config(),
        toolsets: config.toolsets.clone(),
        encryption: Default::default(),
        sandbox: config.sandbox.clone(),
        github_app: github_app_config,
        library: config.library.clone(),
        git_proxy: config.git_proxy.clone(),
        tunnel_runtime,
    };

    let app = domain::App::init(&pool, app_config).await?;
    let app_for_shutdown = app.clone();
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
    app_state.dev_mode_agent_tokens = auth_config.dev_mode_agent_tokens;
    app_state.model_options = config
        .providers
        .iter()
        .flat_map(|p| p.models.iter().map(|m| m.name.clone()))
        .collect();
    if app_state.dev_mode_agent_tokens {
        tracing::warn!(
            "dev_mode_agent_tokens enabled — accepting `dev-agent:<uuid>` bearer tokens. \
             NEVER enable in production."
        );
    }

    // Audience must match `sandbox.saTokenAudience` in values.yaml; if
    // they diverge, every TokenReview call fails. Configurable via env
    // for ops cutovers (renaming the audience without a code change).
    let sa_audience =
        std::env::var("DRUA_SA_TOKEN_AUDIENCE").unwrap_or_else(|_| "galoy-agents-git".to_string());
    if let Some(validator) = auth::sa_token::SaTokenValidator::try_from_env(&sa_audience).await {
        tracing::info!(audience = %sa_audience, "SA token validator initialized (in-cluster)");
        app_state = app_state.with_sa_token_validator(validator);
    }

    let router = server::build_app(&server_config, app_state, mcp_service);

    let addr: std::net::SocketAddr =
        format!("{}:{}", config.server.host, config.server.port).parse()?;
    tracing::info!("Starting server on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    tracing::info!("shutdown signal received; draining background jobs");
    app_for_shutdown.shutdown().await;

    if let Err(e) = drua_tracing_utils::shutdown_tracer() {
        eprintln!("Error shutting down tracer: {e}");
    }

    Ok(())
}

/// Resolves on the first SIGINT (ctrl-c) or SIGTERM. axum stops accepting new
/// connections; in-flight requests get to finish; then `App::shutdown` reschedules
/// running jobs so the lost-job detector doesn't have to reap them.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "failed to install ctrl_c handler");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
