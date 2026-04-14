use crate::primitives::{AgentId, AuthSubject};

use super::error::SlashCommandError;

/// Context passed to a slash command when it is executed.
#[derive(Debug, Clone)]
pub struct SlashCommandContext {
    pub subject: AuthSubject,
    pub agent_id: AgentId,
}

/// Output produced by a slash command.
#[derive(Debug, Clone)]
pub enum SlashCommandOutput {
    /// Plain text response sent back to the user.
    Text(String),
}

/// A user-invoked slash command (e.g. `/ping`, `/help`).
///
/// Unlike [`TopLevelTool`](crate::toolset::TopLevelTool) which is invoked by
/// the LLM, slash commands are typed by the user in the chat UI and handled
/// entirely server-side without any LLM interaction.
pub trait SlashCommand: Send + Sync {
    /// The command name without the leading `/` (e.g. `"ping"`).
    fn name(&self) -> &str;

    /// One-line description shown in `/help` output.
    fn description(&self) -> &str;

    /// Execute the command and return output.
    fn execute(
        &self,
        ctx: &SlashCommandContext,
        args: &str,
    ) -> Result<SlashCommandOutput, SlashCommandError>;
}
