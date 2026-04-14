mod commands;
pub mod error;
mod traits;

use std::collections::HashMap;

use crate::primitives::{AgentId, AuthSubject, ChatOutputEvent, UserMessageSource};

pub use error::SlashCommandError;
pub use traits::{SlashCommand, SlashCommandContext, SlashCommandOutput};

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
    pub async fn execute(
        &self,
        name: &str,
        ctx: &SlashCommandContext,
        args: &str,
    ) -> Result<SlashCommandOutput, SlashCommandError> {
        let cmd = self
            .find(name)
            .ok_or_else(|| SlashCommandError::NotFound(name.to_string()))?;
        cmd.execute(ctx, args).await
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

    /// Returns `true` if `prompt` looks like a slash command (`/word`).
    pub fn is_slash_command(prompt: &str) -> bool {
        let trimmed = prompt.trim();
        if !trimmed.starts_with('/') {
            return false;
        }
        // Must have at least one char after `/`
        trimmed.len() > 1 && !trimmed[1..].starts_with(char::is_whitespace)
    }

    /// Process a slash command prompt end-to-end: parse, execute, and return
    /// an event stream matching the shape of `Agents::send_message()`.
    ///
    /// The caller should already have verified [`Self::is_slash_command`].
    #[tracing::instrument(name = "slash_command.process", skip_all)]
    pub async fn process(
        &self,
        source: UserMessageSource,
        agent_id: AgentId,
        subject: &AuthSubject,
        prompt: String,
    ) -> tokio::sync::mpsc::Receiver<ChatOutputEvent> {
        let trimmed = prompt.trim();
        let without_slash = &trimmed[1..];
        let (name, args) = match without_slash.split_once(char::is_whitespace) {
            Some((n, a)) => (n, a.trim()),
            None => (without_slash, ""),
        };

        let ctx = SlashCommandContext {
            subject: subject.clone(),
            agent_id,
        };

        let response_text = match self.execute(name, &ctx, args).await {
            Ok(SlashCommandOutput::Text(t)) => t,
            Err(SlashCommandError::NotFound(_)) => {
                let available: Vec<_> = self
                    .list()
                    .into_iter()
                    .map(|(n, _)| format!("/{n}"))
                    .collect();
                format!(
                    "Unknown command: /{name}. Available commands: {}",
                    available.join(", ")
                )
            }
            Err(e) => format!("Command error: {e}"),
        };

        let (tx, rx) = tokio::sync::mpsc::channel::<ChatOutputEvent>(4);
        let _ = tx
            .send(ChatOutputEvent::UserMessage {
                source,
                text: prompt,
            })
            .await;
        let _ = tx
            .send(ChatOutputEvent::SlashCommandOutput {
                text: response_text,
            })
            .await;
        let _ = tx
            .send(ChatOutputEvent::AssistantDone {
                turns: 0,
                input_tokens: 0,
                output_tokens: 0,
                duration_ms: None,
                cost_usd: None,
            })
            .await;
        rx
    }
}

impl Default for SlashCommands {
    fn default() -> Self {
        Self::new()
    }
}

/// Create a [`SlashCommands`] registry pre-loaded with the built-in commands.
pub fn default_registry() -> SlashCommands {
    let mut registry = SlashCommands::new();
    registry.register(commands::PingCommand);
    registry
}
