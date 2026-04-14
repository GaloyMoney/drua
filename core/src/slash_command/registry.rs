use std::collections::HashMap;

use super::error::SlashCommandError;
use super::traits::{SlashCommand, SlashCommandContext, SlashCommandOutput};

/// Registry of available slash commands.
///
/// Commands are registered at startup and looked up by name when a user
/// message starts with `/`.
pub struct SlashCommands {
    commands: HashMap<String, Box<dyn SlashCommand>>,
}

impl SlashCommands {
    pub fn new() -> Self {
        Self {
            commands: HashMap::new(),
        }
    }

    /// Register a command. Overwrites any existing command with the same name.
    pub fn register(&mut self, cmd: impl SlashCommand + 'static) {
        self.commands.insert(cmd.name().to_string(), Box::new(cmd));
    }

    /// Look up a command by name (without the leading `/`).
    pub fn find(&self, name: &str) -> Option<&dyn SlashCommand> {
        self.commands.get(name).map(|c| c.as_ref())
    }

    /// Execute a command by name, returning an error if the command is not found.
    pub fn execute(
        &self,
        name: &str,
        ctx: &SlashCommandContext,
        args: &str,
    ) -> Result<SlashCommandOutput, SlashCommandError> {
        let cmd = self
            .find(name)
            .ok_or_else(|| SlashCommandError::NotFound(name.to_string()))?;
        cmd.execute(ctx, args)
    }

    /// List all registered commands as `(name, description)` pairs.
    pub fn list(&self) -> Vec<(&str, &str)> {
        let mut items: Vec<_> = self
            .commands
            .values()
            .map(|c| (c.name(), c.description()))
            .collect();
        items.sort_by_key(|(name, _)| *name);
        items
    }
}

impl Default for SlashCommands {
    fn default() -> Self {
        Self::new()
    }
}
