mod web;

use clap::Parser;

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

    let web_state = web::AppState { users, agents };

    let app = web::router(web_state)
        .route(
            "/graphql",
            axum::routing::get(mcp_gateway::graphql::graphql_playground)
                .post(mcp_gateway::graphql::graphql_handler),
        )
        .layer(axum::Extension(schema));

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], cli.port));
    tracing::info!("Starting server on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
