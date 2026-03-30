mod commands;
mod config;
mod indexer;
mod tui;
mod walker;

use clap::{Parser, Subcommand};

/// Code Assistant - MCP server for code style suggestions
#[derive(Parser, Debug)]
#[command(
    name = "code-assistant",
    about = "Code assistant — search and suggestions"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Clone repos, start services, pull model, index everything
    Bootstrap,

    /// Build index from pre-cloned repos on disk (for CI)
    BuildIndex {
        /// Directory containing repo subdirectories
        #[arg(long)]
        repos_dir: String,
    },

    /// Re-index one or all repos from config
    Reindex {
        /// Repo name to re-index (omit to re-index all)
        repo_name: Option<String>,
    },

    /// Show service health and index stats
    Status,

    /// Apply heuristic labels to all indexed chunks
    Label,

    /// Index a local directory by path (manual / ad-hoc)
    Index {
        /// Path to the repository to index
        repo_path: String,

        /// Name to identify this repo in the store
        #[arg(long)]
        name: String,

        /// Database name override (defaults to "code-assistant")
        #[arg(long)]
        collection: Option<String>,
    },

    /// Review and correct heuristic labels on code chunks (TUI)
    Review {
        /// Re-review only chunks flagged by audit-labels
        #[arg(long)]
        confused: bool,
    },

    /// Mine PR review comments from GitHub repos for anti-pattern extraction
    MineReviews,

    /// Run the Telegram bot for label review
    TelegramReview {
        /// Re-review only chunks flagged by audit-labels
        #[arg(long)]
        confused: bool,
    },

    /// Replay human labels from JSONL onto stored data
    ReplayLabels,

    /// Evaluate heuristic classifier accuracy against human labels
    Eval,

    /// Evaluate ONNX classifier accuracy against human labels (side-by-side with heuristic)
    EvalMl,

    /// Audit human labels using ML classifier — flag disagreements for re-review
    AuditLabels,

    /// Classify code using the ONNX SetFit model
    Classify {
        /// Code text to classify (reads from stdin if omitted)
        #[arg(long)]
        text: Option<String>,

        /// Path to ONNX model directory (default: <data_dir>/models/onnx)
        #[arg(long)]
        model_dir: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let config = config::load_config()?;

    match cli.command {
        Command::Bootstrap => commands::bootstrap::run(&config).await,
        Command::BuildIndex { repos_dir } => commands::build_index::run(&config, &repos_dir).await,
        Command::Reindex { repo_name } => {
            commands::reindex::run(&config, repo_name.as_deref()).await
        }
        Command::Status => commands::status::run(&config).await,
        Command::Label => commands::label::run(&config),
        Command::Index {
            repo_path,
            name,
            collection,
        } => commands::index::run(&config, &repo_path, &name, collection.as_deref()).await,
        Command::Review { confused } => commands::review::run(&config, confused),
        Command::MineReviews => commands::mine_reviews::run(&config).await,
        Command::TelegramReview { confused } => {
            commands::telegram_review::run(&config, confused).await
        }
        Command::ReplayLabels => commands::replay_labels::run(&config),
        Command::Eval => commands::eval::run(&config),
        Command::EvalMl => commands::eval_ml::run(&config),
        Command::AuditLabels => commands::audit_labels::run(&config),
        Command::Classify { text, model_dir } => commands::classify::run(&config, text, model_dir),
    }
}
