mod commands;
pub mod error;
mod registry;
mod traits;

pub use error::SlashCommandError;
pub use registry::SlashCommands;
pub use traits::{SlashCommand, SlashCommandContext, SlashCommandOutput};

/// Create a [`SlashCommands`] registry pre-loaded with the built-in commands.
pub fn default_registry() -> SlashCommands {
    let mut registry = SlashCommands::new();
    registry.register(commands::PingCommand);
    registry
}
