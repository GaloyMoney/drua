mod web;

use clap::Parser;
use tower_http::cors::CorsLayer;
use tower_sessions::SessionManagerLayer;

use mcp_gateway::auth::{self, session_store::PgSessionStore, AuthConfig};

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

    let users = mcp_gateway::user::Users::new(&pool);
    let agents = mcp_gateway::agent::Agents::new(&pool);

    let schema = mcp_gateway::graphql::schema(users.clone(), agents.clone());

    // Session store + layer
    let session_store = PgSessionStore::new(&pool);
    let session_layer = SessionManagerLayer::new(session_store);

    // Auth state (OAuth + services for bearer token resolution)
    let auth_config = AuthConfig::from_env();
    let oauth_client = auth_config.oauth_client();
    let auth_state = auth::AppState::new(users.clone(), agents.clone(), oauth_client);

    // Auth routes (GitHub OAuth) need AppState as router state
    let auth_routes = auth::auth_router().with_state(auth_state.clone());

    // Web frontend state
    let web_state = web::AppState { users, agents };

    let app = axum::Router::new()
        .route(
            "/graphql",
            axum::routing::get(mcp_gateway::graphql::graphql_playground)
                .post(mcp_gateway::graphql::graphql_handler),
        )
        .merge(auth_routes)
        .merge(web::router(web_state))
        .layer(axum::Extension(schema))
        .layer(auth::AuthLayer::new(auth_state))
        .layer(session_layer)
        .layer(CorsLayer::permissive());

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], cli.port));
    tracing::info!("Starting server on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
