use clap::Parser;
use tower_sessions::SessionManagerLayer;

use galoy_agents_web::auth::{config::AuthConfig, session_store::PgSessionStore};

#[derive(Parser)]
#[command(name = "galoy-agents", about = "Galoy Agents CLI")]
struct Cli {
    /// Port to listen on
    #[arg(long, env = "GALOY_AGENTS_PORT", default_value = "4200")]
    port: u16,

    /// PostgreSQL connection URL
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,
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

    let pool = sqlx::PgPool::connect(&cli.database_url).await?;
    sqlx::migrate!("../domain/migrations").run(&pool).await?;

    let users = galoy_agents_domain::user::Users::new(&pool);
    let agents = galoy_agents_domain::agent::Agents::new(&pool);

    let auth_config = AuthConfig::from_env();
    let oauth_client = auth_config.oauth_client();

    let app_state = galoy_agents_web::AppState::new(users.clone(), agents.clone(), oauth_client);

    let schema = galoy_agents_graphql::schema(users, agents);

    let session_store = PgSessionStore::new(&pool);
    let session_layer = SessionManagerLayer::new(session_store);

    let app = galoy_agents_web::router()
        .route(
            "/graphql",
            axum::routing::get(galoy_agents_graphql::graphql_playground)
                .post(galoy_agents_graphql::graphql_handler),
        )
        .layer(axum::Extension(schema))
        .layer(axum::middleware::from_fn(
            galoy_agents_web::auth::auth_middleware,
        ))
        .layer(axum::Extension(app_state.clone()))
        .layer(session_layer)
        .with_state(app_state);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], cli.port));
    tracing::info!("Starting server on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
