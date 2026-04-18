mod commands;
mod config;
mod graphql;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "caro", about = "Galoy Agents CLI")]
struct Cli {
    /// Server URL (can also be set during login)
    #[arg(long, env = "CARO_SERVER_URL")]
    server: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Authenticate with a galoy-agents server
    Login,
    /// Show current connection status
    Status,
    /// Remove stored credentials
    Logout,
    /// Workspace management
    Workspace {
        #[command(subcommand)]
        action: WorkspaceAction,
    },
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
        Command::Login => commands::login::run(cli.server).await,
        Command::Status => commands::status::run().await,
        Command::Logout => commands::logout::run(),
        Command::Workspace { action } => match action {
            WorkspaceAction::List => commands::workspace::list().await,
            WorkspaceAction::Create { name, description } => {
                commands::workspace::create(&name, description.as_deref()).await
            }
            WorkspaceAction::Show { id } => commands::workspace::show(&id).await,
        },
    }
}
