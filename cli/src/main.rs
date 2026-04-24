use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "drua", about = "Drua — AI agent workspaces")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server
    Server {
        #[command(subcommand)]
        action: Option<ServerAction>,

        /// Path to config file
        #[arg(long, env = "DRUA_CONFIG", default_value = "drua.yml")]
        config: String,

        /// PostgreSQL connection URL
        #[arg(long, env = "PG_CON", default_value = "")]
        pg_con: String,

        /// GitHub OAuth client secret
        #[arg(long, env = "GITHUB_CLIENT_SECRET", default_value = "")]
        github_client_secret: String,

        /// Comma-separated list of allowed GitHub teams (org/team-slug format)
        #[arg(long, env = "GITHUB_ALLOWED_TEAMS", default_value = "")]
        github_allowed_teams: String,

        /// Anthropic API key
        #[arg(long, env = "ANTHROPIC_API_KEY", default_value = "")]
        anthropic_api_key: String,

        /// OpenAI API key
        #[arg(long, env = "OPENAI_API_KEY", default_value = "")]
        openai_api_key: String,

        /// Override YAML config values (dot-separated paths, e.g. --set oauth.login=dev)
        #[clap(long = "set", value_name = "KEY=VALUE")]
        config_overrides: Vec<String>,
    },

    /// Interactive TUI
    Tui {
        /// Server URL (default: http://localhost:4200)
        #[arg(long, env = "DRUA_SERVER_URL")]
        server: Option<String>,
    },

    /// Authenticate with a drua server
    Login {
        /// Server URL (default: http://localhost:4200)
        #[arg(long, env = "DRUA_SERVER_URL")]
        server: Option<String>,
    },

    /// Show current connection status
    Status,

    /// Remove stored credentials
    Logout,

    /// Chat with an agent interactively
    Chat {
        /// Server URL (default: http://localhost:4200)
        #[arg(long, env = "DRUA_SERVER_URL")]
        server: Option<String>,

        /// Agent ID to chat with (default: auto-provisioned workspace lead)
        #[arg(long)]
        agent: Option<String>,
    },

    /// Export an agent's thread as Pi-compatible JSONL
    Export {
        /// Agent ID (UUID)
        agent_id: String,
    },

    /// Workspace management
    Workspace {
        #[command(subcommand)]
        action: WorkspaceAction,
    },
}

#[derive(Subcommand)]
enum ServerAction {
    /// Generate default configuration file (drua.yml) with all default values
    DumpDefaultConfig,
}

#[derive(Subcommand)]
enum WorkspaceAction {
    /// List all workspaces
    List,
    /// Create a new workspace
    Create {
        /// Workspace name
        name: String,
        /// Optional description
        #[arg(long)]
        description: Option<String>,
    },
    /// Show workspace details
    Show {
        /// Workspace ID (UUID)
        id: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Server {
            action,
            config,
            pg_con,
            github_client_secret,
            github_allowed_teams,
            anthropic_api_key,
            openai_api_key,
            config_overrides,
        } => match action {
            Some(ServerAction::DumpDefaultConfig) => drua_server::dump_default_config(),
            None => {
                drua_server::run_server(drua_server::RunServerArgs {
                    config_path: config,
                    pg_con,
                    github_client_secret,
                    github_allowed_teams,
                    anthropic_api_key,
                    openai_api_key,
                    config_overrides,
                })
                .await
            }
        },

        Command::Tui { server } => drua_client::commands::tui::run(server).await,
        Command::Login { server } => drua_client::commands::login::run(server).await,
        Command::Status => drua_client::commands::status::run().await,
        Command::Logout => drua_client::commands::logout::run(),
        Command::Chat { server, agent } => drua_client::commands::chat::run(server, agent).await,
        Command::Export { agent_id } => drua_client::commands::export::run(&agent_id).await,
        Command::Workspace { action } => match action {
            WorkspaceAction::List => drua_client::commands::workspace::list().await,
            WorkspaceAction::Create { name, description } => {
                drua_client::commands::workspace::create(&name, description.as_deref()).await
            }
            WorkspaceAction::Show { id } => drua_client::commands::workspace::show(&id).await,
        },
    }
}
